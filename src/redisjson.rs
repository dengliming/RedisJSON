// RedisJSON Redis module.
//
// Translate between JSON and tree of Redis objects:
// User-provided JSON is converted to a tree. This tree is stored transparently in Redis.
// It can be operated on (e.g. INCR) and serialized back to JSON.

use crate::backward;
use crate::error::Error;
use crate::formatter::RedisJsonFormatter;
use crate::nodevisitor::{StaticPathElement, StaticPathParser, VisitStatus};
use crate::REDIS_JSON_TYPE_VERSION;

use bson::decode_document;
use jsonpath_lib::SelectorMut;
use redis_module::raw::{self, Status};
use serde::Serialize;
use serde_json::{Map, Value};
use std::io::Cursor;
use std::mem;
use std::os::raw::{c_int, c_void};

#[derive(Debug, PartialEq)]
pub enum SetOptions {
    NotExists,
    AlreadyExists,
    None,
}

#[derive(Debug, PartialEq)]
pub enum Format {
    JSON,
    BSON,
}
impl Format {
    pub fn from_str(s: &str) -> Result<Format, Error> {
        match s {
            "JSON" => Ok(Format::JSON),
            "BSON" => Ok(Format::BSON),
            _ => Err("ERR wrong format".into()),
        }
    }
}

///
/// Backwards compatibility convertor for RedisJSON 1.x clients
///
pub struct Path {
    pub path: String,
    pub fixed: String,
}

impl Path {
    pub fn new(path: String) -> Path {
        let mut fixed = path.clone();
        if !fixed.starts_with('$') {
            if fixed == "." {
                fixed.replace_range(..1, "$");
            } else if fixed.starts_with('.') {
                fixed.insert(0, '$');
            } else {
                fixed.insert_str(0, "$.");
            }
        }
        Path { path, fixed }
    }
}

#[derive(Debug)]
pub struct RedisJSON {
    data: Value,
}

impl RedisJSON {
    pub fn parse_str(data: &str, format: Format) -> Result<Value, Error> {
        match format {
            Format::JSON => Ok(serde_json::from_str(data)?),
            Format::BSON => decode_document(&mut Cursor::new(data.as_bytes()))
                .map(|docs| {
                    let v = if !docs.is_empty() {
                        docs.iter()
                            .next()
                            .map_or_else(|| Value::Null, |(_, b)| b.clone().into())
                    } else {
                        Value::Null
                    };
                    Ok(v)
                })
                .unwrap_or_else(|e| Err(e.to_string().into())),
        }
    }

    pub fn from_str(data: &str, format: Format) -> Result<Self, Error> {
        let value = RedisJSON::parse_str(data, format)?;
        Ok(Self { data: value })
    }

    fn add_value(&mut self, path: &str, value: Value) -> Result<bool, Error> {
        let mut parsed_static_path = StaticPathParser::check(path)?;

        if parsed_static_path.valid != VisitStatus::Valid {
            return Err("Err: wrong static path".into());
        }
        if parsed_static_path.static_path_elements.len() < 2 {
            return Err("Err: path must end with object key to set".into());
        }

        if let StaticPathElement::ObjectKey(key) =
            parsed_static_path.static_path_elements.pop().unwrap()
        {
            let mut current_data = self.data.take();
            if let StaticPathElement::Root = parsed_static_path.static_path_elements.last().unwrap()
            {
                // Adding to the root, can't use jsonpath_lib::replace_with
                let res = if let Value::Object(ref mut map) = current_data {
                    if map.contains_key(&key) {
                        false
                    } else {
                        map.insert(key, value);
                        true
                    }
                } else {
                    false
                };
                self.data = current_data;
                Ok(res)
            } else {
                // Adding somewhere in existing object, use jsonpath_lib::replace_with
                let mut set = false;
                self.data = jsonpath_lib::replace_with(
                    current_data,
                    &parsed_static_path
                        .static_path_elements
                        .iter()
                        .map(|e| e.to_string())
                        .collect::<Vec<String>>()
                        .join(""),
                    &mut |mut ret| {
                        if let Value::Object(ref mut map) = ret {
                            if map.contains_key(&key) {
                                set = false;
                            } else {
                                map.insert(key.to_string(), value.clone());
                                set = true;
                            }
                        }
                        Some(ret)
                    },
                )?;
                Ok(set)
            }
        } else {
            Err("Err: path not an object".into())
        }
    }

    pub fn set_value(
        &mut self,
        data: &str,
        path: &str,
        option: &SetOptions,
        format: Format,
    ) -> Result<bool, Error> {
        let json: Value = RedisJSON::parse_str(data, format)?;
        if path == "$" {
            if SetOptions::NotExists == *option {
                Ok(false)
            } else {
                self.data = json;
                Ok(true)
            }
        } else {
            let mut replaced = false;
            if SetOptions::NotExists != *option {
                let current_data = self.data.take();
                self.data = jsonpath_lib::replace_with(current_data, path, &mut |_v| {
                    replaced = true;
                    Some(json.clone())
                })?;
            }
            if replaced {
                Ok(true)
            } else if SetOptions::AlreadyExists != *option {
                self.add_value(path, json)
            } else {
                Ok(false)
            }
        }
    }

    pub fn delete_path(&mut self, path: &str) -> Result<usize, Error> {
        let current_data = self.data.take();

        let mut deleted = 0;
        self.data = jsonpath_lib::replace_with(current_data, path, &mut |v| {
            if !v.is_null() {
                deleted += 1; // might delete more than a single value
            }
            None
        })?;
        Ok(deleted)
    }

    pub fn clear(&mut self, path: &str) -> Result<usize, Error> {
        let current_data = self.data.take();
        let mut cleared = 0;

        let clear_func = &mut |v| match v {
            Value::Object(mut obj) => {
                obj.clear();
                cleared += 1;
                Some(Value::from(obj))
            }
            Value::Array(mut arr) => {
                arr.clear();
                cleared += 1;
                Some(Value::from(arr))
            }
            _ => Some(v),
        };

        self.data = if path == "$" {
            clear_func(current_data).unwrap()
        } else {
            jsonpath_lib::replace_with(current_data, path, clear_func)?
        };
        Ok(cleared)
    }
    pub fn to_string(&self, path: &str, format: Format) -> Result<String, Error> {
        let results = self.get_first(path)?;
        Self::serialize(results, format)
    }

    pub fn serialize(results: &Value, format: Format) -> Result<String, Error> {
        let res = match format {
            Format::JSON => serde_json::to_string(results)?,
            Format::BSON => return Err("Soon to come...".into()), //results.into() as Bson,
        };
        Ok(res)
    }

    pub fn to_json(
        &self,
        paths: &mut Vec<Path>,
        indent: String,
        newline: String,
        space: String,
        format: Format,
    ) -> Result<String, Error> {
        let temp_doc;
        let res = if paths.len() > 1 {
            let mut selector = jsonpath_lib::selector(&self.data);
            // TODO: Creating a temp doc here duplicates memory usage. This can be very memory inefficient.
            // A better way would be to create a doc of references to the original doc but no current support
            // in serde_json. I'm going for this implementation anyway because serde_json isn't supposed to be
            // memory efficient and we're using it anyway. See https://github.com/serde-rs/json/issues/635.
            temp_doc = Value::Object(paths.drain(..).fold(Map::new(), |mut acc, path| {
                let value = match selector(&path.fixed) {
                    Ok(s) => match s.first() {
                        Some(v) => v,
                        None => &Value::Null,
                    },
                    Err(_) => &Value::Null,
                };
                acc.insert(path.path, (*value).clone());
                acc
            }));
            &temp_doc
        } else {
            self.get_first(&paths[0].fixed)?
        };

        match format {
            Format::JSON => {
                let formatter = RedisJsonFormatter::new(
                    indent.as_bytes(),
                    space.as_bytes(),
                    newline.as_bytes(),
                );

                let mut out = serde_json::Serializer::with_formatter(Vec::new(), formatter);
                res.serialize(&mut out).unwrap();
                Ok(String::from_utf8(out.into_inner()).unwrap())
            }
            Format::BSON => Err("Soon to come...".into()), //results.into() as Bson,
        }
    }

    pub fn str_len(&self, path: &str) -> Result<usize, Error> {
        self.get_first(path)?
            .as_str()
            .ok_or_else(|| "ERR wrong type of path value".into())
            .map(|s| s.len())
    }

    pub fn arr_len(&self, path: &str) -> Result<usize, Error> {
        self.get_first(path)?
            .as_array()
            .ok_or_else(|| "ERR wrong type of path value".into())
            .map(|arr| arr.len())
    }

    pub fn obj_len(&self, path: &str) -> Result<usize, Error> {
        self.get_first(path)?
            .as_object()
            .ok_or_else(|| "ERR wrong type of path value".into())
            .map(|obj| obj.len())
    }

    pub fn obj_keys<'a>(&'a self, path: &'a str) -> Result<Vec<&'a String>, Error> {
        self.get_first(path)?
            .as_object()
            .ok_or_else(|| "ERR wrong type of path value".into())
            .map(|obj| obj.keys().collect())
    }

    pub fn arr_index(&self, path: &str, scalar: &str, start: i64, end: i64) -> Result<i64, Error> {
        if let Value::Array(arr) = self.get_first(path)? {
            // end=-1/0 means INFINITY to support backward with RedisJSON
            if arr.is_empty() || end < -1 {
                return Ok(-1);
            }
            let v: Value = serde_json::from_str(scalar)?;

            let len = arr.len() as i64;

            // Normalize start
            let start = if start < 0 {
                0.max(len + start)
            } else {
                // start >= 0
                start.min(len - 1)
            };

            // Normalize end
            let end = if end == 0 {
                len
            } else if end < 0 {
                len + end
            } else {
                // end > 0
                end.min(len)
            };

            if end < start {
                // don't search at all
                return Ok(-1);
            }

            let slice = &arr[start as usize..end as usize];

            match slice.iter().position(|r| r == &v) {
                Some(i) => Ok((start as usize + i) as i64),
                None => Ok(-1),
            }
        } else {
            Ok(-1)
        }
    }

    pub fn get_type(&self, path: &str) -> Result<String, Error> {
        let s = RedisJSON::value_name(self.get_first(path)?);
        Ok(s.to_string())
    }

    pub fn value_name(value: &Value) -> &str {
        match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(n) => {
                if n.is_f64() {
                    "number"
                } else {
                    "integer"
                }
            }
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    pub fn value_op<F, R, T>(&mut self, path: &str, mut op_fun: F, res_func: R) -> Result<T, Error>
    where
        F: FnMut(&mut Value) -> Result<Value, Error>,
        R: Fn(&Value) -> Result<T, Error>,
    {
        // take the root before updating the value must be returned at the end
        let current_data = self.data.take();

        let mut errors = vec![];
        let mut result = None;

        // A wrapper function that is called by replace_with
        // calls op_fun and then res_func
        let mut collect_fun = |mut value: Value| {
            op_fun(&mut value)
                .and_then(|new_value| {
                    // after calling op_fun calling res_func
                    // to prepae the command result
                    res_func(&new_value).map(|res| {
                        result = Some(res);
                        new_value
                    })
                })
                .map_err(|e| {
                    errors.push(e);
                })
                .unwrap_or(value)
        };

        if path == "$" {
            // root needs special handling
            self.data = collect_fun(current_data)
        } else {
            match SelectorMut::new().str_path(path) {
                Ok(selector) => {
                    let replace_result = selector
                        .value(current_data)
                        .replace_with(&mut |v| Some(collect_fun(v)));

                    if let Err(e) = replace_result {
                        errors.push(e.into());
                    }
                    // reassign the modified root
                    self.data = selector.take().unwrap();
                }
                Err(e) => {
                    errors.push(e.into());
                    // reassign the original root
                    self.data = current_data;
                }
            }
        };

        match errors.len() {
            0 => match result {
                Some(r) => Ok(r),
                None => Err(format!("Path '{}' does not exist", path).into()),
            },
            1 => Err(errors.remove(0)),
            _ => Err(errors.into_iter().map(|e| e.msg).collect::<String>().into()),
        }
    }

    pub fn get_memory<'a>(&'a self, path: &'a str) -> Result<usize, Error> {
        // TODO add better calculation, handle wrappers, internals and length
        let res = match self.get_first(path)? {
            Value::Null => 0,
            Value::Bool(v) => mem::size_of_val(v),
            Value::Number(v) => mem::size_of_val(v),
            Value::String(v) => mem::size_of_val(v),
            Value::Array(v) => mem::size_of_val(v),
            Value::Object(v) => mem::size_of_val(v),
        };
        Ok(res)
    }

    pub fn get_first<'a>(&'a self, path: &'a str) -> Result<&'a Value, Error> {
        let results = self.get_values(path)?;
        match results.first() {
            Some(s) => Ok(s),
            None => Err("ERR path does not exist".into()),
        }
    }

    pub fn get_values<'a>(&'a self, path: &'a str) -> Result<Vec<&'a Value>, Error> {
        let results = jsonpath_lib::select(&self.data, path)?;
        Ok(results)
    }
}

pub mod type_methods {
    use super::*;

    #[allow(non_snake_case, unused)]
    pub extern "C" fn rdb_load(rdb: *mut raw::RedisModuleIO, encver: c_int) -> *mut c_void {
        let json = match encver {
            0 => RedisJSON {
                data: backward::json_rdb_load(rdb),
            },
            2 => {
                let data = raw::load_string(rdb);
                // Backward support for modules that had AUX field for RediSarch
                // TODO remove in future versions
                if raw::load_unsigned(rdb) > 0 {
                    raw::load_string(rdb);
                    raw::load_string(rdb);
                }
                RedisJSON::from_str(&data, Format::JSON).unwrap()
            }
            3 => {
                let data = raw::load_string(rdb);
                RedisJSON::from_str(&data, Format::JSON).unwrap()
            }
            _ => panic!("Can't load old RedisJSON RDB"),
        };
        Box::into_raw(Box::new(json)) as *mut c_void
    }

    #[allow(non_snake_case, unused)]
    pub unsafe extern "C" fn free(value: *mut c_void) {
        let json = value as *mut RedisJSON;

        // Take ownership of the data from Redis (causing it to be dropped when we return)
        Box::from_raw(json);
    }

    #[allow(non_snake_case, unused)]
    pub unsafe extern "C" fn rdb_save(rdb: *mut raw::RedisModuleIO, value: *mut c_void) {
        let json = &*(value as *mut RedisJSON);
        raw::save_string(rdb, &json.data.to_string());
    }

    #[allow(non_snake_case, unused)]
    pub unsafe extern "C" fn aux_load(rdb: *mut raw::RedisModuleIO, encver: i32, when: i32) -> i32 {
        if (encver > REDIS_JSON_TYPE_VERSION) {
            return Status::Err as i32; // could not load rdb created with higher RedisJSON version!
        }

        // Backward support for modules that had AUX field for RediSarch
        // TODO remove in future versions
        if (encver == 2 && when == raw::Aux::Before as i32) {
            let map_size = raw::load_unsigned(rdb);
            for _ in 0..map_size {
                let index_name = raw::load_string(rdb);
                let fields_size = raw::load_unsigned(rdb);
                for _ in 0..fields_size {
                    let field_name = raw::load_string(rdb);
                    let path = raw::load_string(rdb);
                    // index::add_field(&index_name, &field_name, &path);
                }
            }
        }

        Status::Ok as i32
    }
}

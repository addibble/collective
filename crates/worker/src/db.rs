//! Thin helpers over D1: positional arguments, typed rows, and errors that
//! map onto HTTP statuses.

use serde::de::DeserializeOwned;
use worker::wasm_bindgen::JsValue;
use worker::{D1Database, D1PreparedStatement};

use crate::error::ApiResult;

pub enum Arg {
    Str(String),
    Num(f64),
    Null,
}

impl From<&str> for Arg {
    fn from(v: &str) -> Self {
        Arg::Str(v.into())
    }
}
impl From<String> for Arg {
    fn from(v: String) -> Self {
        Arg::Str(v)
    }
}
impl From<&String> for Arg {
    fn from(v: &String) -> Self {
        Arg::Str(v.clone())
    }
}
impl From<f64> for Arg {
    fn from(v: f64) -> Self {
        Arg::Num(v)
    }
}
impl From<i64> for Arg {
    fn from(v: i64) -> Self {
        Arg::Num(v as f64)
    }
}
impl From<u32> for Arg {
    fn from(v: u32) -> Self {
        Arg::Num(v as f64)
    }
}
impl From<usize> for Arg {
    fn from(v: usize) -> Self {
        Arg::Num(v as f64)
    }
}
impl From<bool> for Arg {
    fn from(v: bool) -> Self {
        Arg::Num(if v { 1.0 } else { 0.0 })
    }
}
impl<T: Into<Arg>> From<Option<T>> for Arg {
    fn from(v: Option<T>) -> Self {
        v.map_or(Arg::Null, Into::into)
    }
}

impl Arg {
    fn js(&self) -> JsValue {
        match self {
            Arg::Str(s) => JsValue::from_str(s),
            Arg::Num(n) => JsValue::from_f64(*n),
            Arg::Null => JsValue::NULL,
        }
    }
}

#[macro_export]
macro_rules! args {
    ($($x:expr),* $(,)?) => { vec![$($crate::db::Arg::from($x)),*] };
}

/// D1 allows at most 100 bound parameters per statement.
pub const MAX_PARAMS: usize = 90;

pub fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

pub struct Db(pub D1Database);

impl Db {
    pub fn stmt(&self, sql: &str, args: Vec<Arg>) -> ApiResult<D1PreparedStatement> {
        let js: Vec<JsValue> = args.iter().map(Arg::js).collect();
        Ok(self.0.prepare(sql).bind(&js)?)
    }

    pub async fn all<T: DeserializeOwned>(&self, sql: &str, args: Vec<Arg>) -> ApiResult<Vec<T>> {
        Ok(self.stmt(sql, args)?.all().await?.results::<T>()?)
    }

    pub async fn first<T: DeserializeOwned>(
        &self,
        sql: &str,
        args: Vec<Arg>,
    ) -> ApiResult<Option<T>> {
        Ok(self.stmt(sql, args)?.first::<T>(None).await?)
    }

    pub async fn run(&self, sql: &str, args: Vec<Arg>) -> ApiResult<()> {
        self.stmt(sql, args)?.run().await?;
        Ok(())
    }

    /// Runs a write and returns how many rows it changed.
    pub async fn changes(&self, sql: &str, args: Vec<Arg>) -> ApiResult<usize> {
        let r = self.stmt(sql, args)?.run().await?;
        Ok(r.meta()?.and_then(|m| m.changes).unwrap_or(0))
    }

    pub async fn batch(&self, stmts: Vec<D1PreparedStatement>) -> ApiResult<()> {
        if !stmts.is_empty() {
            self.0.batch(stmts).await?;
        }
        Ok(())
    }
}

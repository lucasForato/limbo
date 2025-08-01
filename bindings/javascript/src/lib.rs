#![deny(clippy::all)]

use std::cell::{RefCell, RefMut};
use std::num::{NonZero, NonZeroUsize};

use std::rc::Rc;
use std::sync::{Arc, OnceLock};

use napi::bindgen_prelude::{JsObjectValue, Null, Object, ToNapiValue};
use napi::{bindgen_prelude::ObjectFinalize, Env, JsValue, Unknown};
use napi_derive::napi;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::EnvFilter;
use turso_core::{LimboError, StepResult};

static TRACING_INIT: OnceLock<()> = OnceLock::new();

fn init_tracing() {
    TRACING_INIT.get_or_init(|| {
        tracing_subscriber::fmt()
            .with_thread_ids(true)
            .with_span_events(FmtSpan::ACTIVE)
            .with_env_filter(EnvFilter::from_default_env())
            .init();
    });
}

#[derive(Default)]
#[napi(object)]
pub struct OpenDatabaseOptions {
    pub readonly: Option<bool>,
    pub file_must_exist: Option<bool>,
    pub timeout: Option<u32>,
    // verbose => Callback,
}

impl OpenDatabaseOptions {
    fn readonly(&self) -> bool {
        self.readonly.unwrap_or(false)
    }
}

#[napi(object)]
pub struct PragmaOptions {
    pub simple: bool,
}

#[napi(object)]
pub struct RunResult {
    pub changes: i64,
    pub last_insert_rowid: i64,
}

#[napi(custom_finalize)]
#[derive(Clone)]
pub struct Database {
    #[napi(writable = false)]
    pub memory: bool,

    #[napi(writable = false)]
    pub readonly: bool,
    // #[napi(writable = false)]
    // pub in_transaction: bool,
    #[napi(writable = false)]
    pub open: bool,
    #[napi(writable = false)]
    pub name: String,
    db: Option<Arc<turso_core::Database>>,
    conn: Arc<turso_core::Connection>,
    _io: Arc<dyn turso_core::IO>,
}

impl ObjectFinalize for Database {
    // TODO: check if something more is required
    fn finalize(self, _env: Env) -> napi::Result<()> {
        self.conn.close().map_err(into_napi_error)?;
        Ok(())
    }
}

#[napi]
impl Database {
    #[napi(constructor)]
    pub fn new(path: String, options: Option<OpenDatabaseOptions>) -> napi::Result<Self, String> {
        init_tracing();

        let memory = path == ":memory:";
        let io: Arc<dyn turso_core::IO> = if memory {
            Arc::new(turso_core::MemoryIO::new())
        } else {
            Arc::new(turso_core::PlatformIO::new().map_err(into_napi_sqlite_error)?)
        };
        let opts = options.unwrap_or_default();
        let flag = if opts.readonly() {
            turso_core::OpenFlags::ReadOnly
        } else {
            turso_core::OpenFlags::Create
        };
        let file = io
            .open_file(&path, flag, false)
            .map_err(|err| into_napi_error_with_message("SQLITE_CANTOPEN".to_owned(), err))?;

        let db_file = Arc::new(DatabaseFile::new(file));
        let db = turso_core::Database::open(io.clone(), &path, db_file, false, false)
            .map_err(into_napi_sqlite_error)?;
        let conn = db.connect().map_err(into_napi_sqlite_error)?;

        Ok(Self {
            readonly: opts.readonly(),
            memory,
            db: Some(db),
            conn,
            open: true,
            name: path,
            _io: io,
        })
    }

    #[napi]
    pub fn prepare(&self, sql: String) -> napi::Result<Statement> {
        let stmt = self.conn.prepare(&sql).map_err(into_napi_error)?;
        Ok(Statement::new(RefCell::new(stmt), self.clone(), sql))
    }

    #[napi]
    pub fn pragma<'env>(
        &self,
        env: &'env Env,
        pragma_name: String,
        options: Option<PragmaOptions>,
    ) -> napi::Result<Unknown<'env>> {
        let sql = format!("PRAGMA {pragma_name}");
        let stmt = self.prepare(sql)?;
        match options {
            Some(PragmaOptions { simple: true, .. }) => {
                let mut stmt = stmt.inner.borrow_mut();
                loop {
                    match stmt.step().map_err(into_napi_error)? {
                        turso_core::StepResult::Row => {
                            let row: Vec<_> = stmt.row().unwrap().get_values().cloned().collect();
                            return to_js_value(env, row[0].clone());
                        }
                        turso_core::StepResult::Done => {
                            return ToNapiValue::into_unknown((), env);
                        }
                        turso_core::StepResult::IO => {
                            stmt.run_once().map_err(into_napi_error)?;
                            continue;
                        }
                        step @ turso_core::StepResult::Interrupt
                        | step @ turso_core::StepResult::Busy => {
                            return Err(napi::Error::new(
                                napi::Status::GenericFailure,
                                format!("{step:?}"),
                            ))
                        }
                    }
                }
            }
            _ => Ok(stmt.run_internal(env, None)?),
        }
    }

    #[napi]
    pub fn backup(&self) {
        todo!()
    }

    #[napi]
    pub fn serialize(&self) {
        todo!()
    }

    #[napi]
    pub fn function(&self) {
        todo!()
    }

    #[napi]
    pub fn aggregate(&self) {
        todo!()
    }

    #[napi]
    pub fn table(&self) {
        todo!()
    }

    #[napi]
    pub fn load_extension(&self, path: String) -> napi::Result<()> {
        let ext_path = turso_core::resolve_ext_path(path.as_str()).map_err(into_napi_error)?;
        #[cfg(not(target_family = "wasm"))]
        {
            self.conn
                .load_extension(ext_path)
                .map_err(into_napi_error)?;
        }
        Ok(())
    }

    #[napi]
    pub fn exec(&self, sql: String) -> napi::Result<(), String> {
        let query_runner = self.conn.query_runner(sql.as_bytes());

        // Since exec doesn't return any values, we can just iterate over the results
        for output in query_runner {
            match output {
                Ok(Some(mut stmt)) => loop {
                    match stmt.step() {
                        Ok(StepResult::Row) => continue,
                        Ok(StepResult::IO) => stmt.run_once().map_err(into_napi_sqlite_error)?,
                        Ok(StepResult::Done) => break,
                        Ok(StepResult::Interrupt | StepResult::Busy) => {
                            return Err(napi::Error::new(
                                "SQLITE_ERROR".to_owned(),
                                "Statement execution interrupted or busy".to_string(),
                            ));
                        }
                        Err(err) => {
                            return Err(napi::Error::new(
                                "SQLITE_ERROR".to_owned(),
                                format!("Error executing SQL: {err}"),
                            ));
                        }
                    }
                },
                Ok(None) => continue,
                Err(err) => {
                    return Err(napi::Error::new(
                        "SQLITE_ERROR".to_owned(),
                        format!("Error executing SQL: {err}"),
                    ));
                }
            }
        }
        Ok(())
    }

    #[napi]
    pub fn close(&mut self) -> napi::Result<()> {
        if self.open {
            self.conn.close().map_err(into_napi_error)?;
            self.db.take();
            self.open = false;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum PresentationMode {
    Raw,
    Pluck,
    None,
}

#[napi]
#[derive(Clone)]
pub struct Statement {
    // TODO: implement each property when core supports it
    // #[napi(able = false)]
    // pub reader: bool,
    // #[napi(writable = false)]
    // pub readonly: bool,
    // #[napi(writable = false)]
    // pub busy: bool,
    #[napi(writable = false)]
    pub source: String,

    database: Database,
    presentation_mode: PresentationMode,
    binded: bool,
    inner: Rc<RefCell<turso_core::Statement>>,
}

#[napi]
impl Statement {
    pub fn new(inner: RefCell<turso_core::Statement>, database: Database, source: String) -> Self {
        Self {
            inner: Rc::new(inner),
            database,
            source,
            presentation_mode: PresentationMode::None,
            binded: false,
        }
    }

    #[napi]
    pub fn get<'env>(
        &self,
        env: &'env Env,
        args: Option<Vec<Unknown>>,
    ) -> napi::Result<Unknown<'env>> {
        let mut stmt = self.check_and_bind(env, args)?;

        loop {
            let step = stmt.step().map_err(into_napi_error)?;
            match step {
                turso_core::StepResult::Row => {
                    let row = stmt.row().unwrap();

                    match self.presentation_mode {
                        PresentationMode::Raw => {
                            let mut raw_obj = env.create_array(row.len() as u32)?;
                            for (idx, value) in row.get_values().enumerate() {
                                let js_value = to_js_value(env, value.clone());

                                raw_obj.set(idx as u32, js_value)?;
                            }
                            return Ok(raw_obj.coerce_to_object()?.to_unknown());
                        }
                        PresentationMode::Pluck => {
                            let (_, value) =
                                row.get_values().enumerate().next().ok_or(napi::Error::new(
                                    napi::Status::GenericFailure,
                                    "Pluck mode requires at least one column in the result",
                                ))?;

                            let result = to_js_value(env, value.clone())?;
                            return ToNapiValue::into_unknown(result, env);
                        }
                        PresentationMode::None => {
                            let mut obj = Object::new(env)?;

                            for (idx, value) in row.get_values().enumerate() {
                                let key = stmt.get_column_name(idx);
                                let js_value = to_js_value(env, value.clone());

                                obj.set_named_property(&key, js_value)?;
                            }

                            return Ok(obj.to_unknown());
                        }
                    }
                }
                turso_core::StepResult::Done => return ToNapiValue::into_unknown((), env),
                turso_core::StepResult::IO => {
                    stmt.run_once().map_err(into_napi_error)?;
                    continue;
                }
                turso_core::StepResult::Interrupt | turso_core::StepResult::Busy => {
                    return Err(napi::Error::new(
                        napi::Status::GenericFailure,
                        format!("{step:?}"),
                    ))
                }
            }
        }
    }

    #[napi]
    pub fn run(&self, env: Env, args: Option<Vec<Unknown>>) -> napi::Result<RunResult> {
        self.run_and_build_info_object(|| self.run_internal(&env, args))
    }

    fn run_internal<'env>(
        &self,
        env: &'env Env,
        args: Option<Vec<Unknown>>,
    ) -> napi::Result<Unknown<'env>> {
        let stmt = self.check_and_bind(env, args)?;

        self.internal_all(env, stmt)
    }

    fn run_and_build_info_object<T, E>(
        &self,
        query_fn: impl FnOnce() -> Result<T, E>,
    ) -> Result<RunResult, E> {
        let total_changes_before = self.database.conn.total_changes();

        query_fn()?;

        let last_insert_rowid = self.database.conn.last_insert_rowid();
        let changes = if self.database.conn.total_changes() == total_changes_before {
            0
        } else {
            self.database.conn.changes()
        };

        Ok(RunResult {
            changes,
            last_insert_rowid,
        })
    }

    #[napi]
    pub fn all<'env>(
        &self,
        env: &'env Env,
        args: Option<Vec<Unknown>>,
    ) -> napi::Result<Unknown<'env>> {
        let stmt = self.check_and_bind(env, args)?;

        self.internal_all(env, stmt)
    }

    fn internal_all<'env>(
        &self,
        env: &'env Env,
        mut stmt: RefMut<'_, turso_core::Statement>,
    ) -> napi::Result<Unknown<'env>> {
        let mut results = env.create_array(1)?;
        let mut index = 0;
        loop {
            match stmt.step().map_err(into_napi_error)? {
                turso_core::StepResult::Row => {
                    let row = stmt.row().unwrap();

                    match self.presentation_mode {
                        PresentationMode::Raw => {
                            let mut raw_array = env.create_array(row.len() as u32)?;
                            for (idx, value) in row.get_values().enumerate() {
                                let js_value = to_js_value(env, value.clone())?;
                                raw_array.set(idx as u32, js_value)?;
                            }
                            results.set_element(index, raw_array.coerce_to_object()?)?;
                            index += 1;
                            continue;
                        }
                        PresentationMode::Pluck => {
                            let (_, value) =
                                row.get_values().enumerate().next().ok_or(napi::Error::new(
                                    napi::Status::GenericFailure,
                                    "Pluck mode requires at least one column in the result",
                                ))?;
                            let js_value = to_js_value(env, value.clone())?;
                            results.set_element(index, js_value)?;
                            index += 1;
                            continue;
                        }
                        PresentationMode::None => {
                            let mut obj = Object::new(env)?;
                            for (idx, value) in row.get_values().enumerate() {
                                let key = stmt.get_column_name(idx);
                                let js_value = to_js_value(env, value.clone());
                                obj.set_named_property(&key, js_value)?;
                            }
                            results.set_element(index, obj)?;
                            index += 1;
                        }
                    }
                }
                turso_core::StepResult::Done => {
                    break;
                }
                turso_core::StepResult::IO => {
                    stmt.run_once().map_err(into_napi_error)?;
                }
                turso_core::StepResult::Interrupt | turso_core::StepResult::Busy => {
                    return Err(napi::Error::new(
                        napi::Status::GenericFailure,
                        format!("{:?}", stmt.step()),
                    ));
                }
            }
        }

        Ok(results.to_unknown())
    }

    #[napi]
    pub fn pluck(&mut self, pluck: Option<bool>) {
        self.presentation_mode = match pluck {
            Some(false) => PresentationMode::None,
            _ => PresentationMode::Pluck,
        };
    }

    #[napi]
    pub fn expand() {
        todo!()
    }

    #[napi]
    pub fn raw(&mut self, raw: Option<bool>) {
        self.presentation_mode = match raw {
            Some(false) => PresentationMode::None,
            _ => PresentationMode::Raw,
        };
    }

    #[napi]
    pub fn columns() {
        todo!()
    }

    #[napi]
    pub fn bind(&mut self, env: Env, args: Option<Vec<Unknown>>) -> napi::Result<Self, String> {
        self.check_and_bind(&env, args)
            .map_err(with_sqlite_error_message)?;
        self.binded = true;

        Ok(self.clone())
    }

    /// Check if the Statement is already binded by the `bind()` method
    /// and bind values to variables.
    fn check_and_bind(
        &self,
        env: &Env,
        args: Option<Vec<Unknown>>,
    ) -> napi::Result<RefMut<'_, turso_core::Statement>> {
        let mut stmt = self.inner.borrow_mut();
        stmt.reset();
        if let Some(args) = args {
            if self.binded {
                let err = napi::Error::new(
                    into_convertible_type_error_message("TypeError"),
                    "The bind() method can only be invoked once per statement object",
                );
                unsafe {
                    napi::JsTypeError::from(err).throw_into(env.raw());
                }

                return Err(napi::Error::from_status(napi::Status::PendingException));
            }

            if args.len() == 1 {
                if matches!(args[0].get_type()?, napi::ValueType::Object) {
                    let obj: Object = args.into_iter().next().unwrap().coerce_to_object()?;

                    if obj.is_array()? {
                        bind_positional_param_array(&mut stmt, &obj)?;
                    } else {
                        bind_host_params(&mut stmt, &obj)?;
                    }
                } else {
                    bind_single_param(&mut stmt, args.into_iter().next().unwrap())?;
                }
            } else {
                bind_positional_params(&mut stmt, args)?;
            }
        }

        Ok(stmt)
    }
}

fn bind_positional_params(
    stmt: &mut RefMut<'_, turso_core::Statement>,
    args: Vec<Unknown>,
) -> Result<(), napi::Error> {
    for (i, elem) in args.into_iter().enumerate() {
        let value = from_js_value(elem)?;
        stmt.bind_at(NonZeroUsize::new(i + 1).unwrap(), value);
    }
    Ok(())
}

fn bind_host_params(
    stmt: &mut RefMut<'_, turso_core::Statement>,
    obj: &Object,
) -> Result<(), napi::Error> {
    if first_key_is_number(obj) {
        bind_numbered_params(stmt, obj)?;
    } else {
        bind_named_params(stmt, obj)?;
    }

    Ok(())
}

fn first_key_is_number(obj: &Object) -> bool {
    Object::keys(obj)
        .iter()
        .flatten()
        .filter(|key| matches!(obj.has_own_property(key), Ok(result) if result))
        .take(1)
        .any(|key| str::parse::<u32>(key).is_ok())
}

fn bind_numbered_params(
    stmt: &mut RefMut<'_, turso_core::Statement>,
    obj: &Object,
) -> Result<(), napi::Error> {
    for key in Object::keys(obj)?.iter() {
        let Ok(param_idx) = str::parse::<u32>(key) else {
            return Err(napi::Error::new(
                napi::Status::GenericFailure,
                "cannot mix numbers and strings",
            ));
        };
        let Some(non_zero) = NonZero::new(param_idx as usize) else {
            return Err(napi::Error::new(
                napi::Status::GenericFailure,
                "numbered parameters cannot be lower than 1",
            ));
        };

        stmt.bind_at(non_zero, from_js_value(obj.get_named_property(key)?)?);
    }
    Ok(())
}

fn bind_named_params(
    stmt: &mut RefMut<'_, turso_core::Statement>,
    obj: &Object,
) -> Result<(), napi::Error> {
    for idx in 1..stmt.parameters_count() + 1 {
        let non_zero_idx = NonZero::new(idx).unwrap();

        let param = stmt.parameters().name(non_zero_idx);
        let Some(name) = param else {
            return Err(napi::Error::from_reason(format!(
                "could not find named parameter with index {idx}"
            )));
        };

        let value = obj.get_named_property::<napi::Unknown>(&name[1..])?;
        stmt.bind_at(non_zero_idx, from_js_value(value)?);
    }

    Ok(())
}

fn bind_positional_param_array(
    stmt: &mut RefMut<'_, turso_core::Statement>,
    obj: &Object,
) -> Result<(), napi::Error> {
    assert!(obj.is_array()?, "bind_array can only be called with arrays");

    for idx in 1..obj.get_array_length()? {
        stmt.bind_at(
            NonZero::new(idx as usize).unwrap(),
            from_js_value(obj.get_element(idx)?)?,
        );
    }

    Ok(())
}

fn bind_single_param(
    stmt: &mut RefMut<'_, turso_core::Statement>,
    obj: napi::Unknown,
) -> Result<(), napi::Error> {
    stmt.bind_at(NonZero::new(1).unwrap(), from_js_value(obj)?);
    Ok(())
}

fn to_js_value<'a>(env: &'a napi::Env, value: turso_core::Value) -> napi::Result<Unknown<'a>> {
    match value {
        turso_core::Value::Null => ToNapiValue::into_unknown(Null, env),
        turso_core::Value::Integer(i) => ToNapiValue::into_unknown(i, env),
        turso_core::Value::Float(f) => ToNapiValue::into_unknown(f, env),
        turso_core::Value::Text(s) => ToNapiValue::into_unknown(s.as_str(), env),
        turso_core::Value::Blob(b) => ToNapiValue::into_unknown(b, env),
    }
}

fn from_js_value(value: Unknown<'_>) -> napi::Result<turso_core::Value> {
    match value.get_type()? {
        napi::ValueType::Undefined | napi::ValueType::Null | napi::ValueType::Unknown => {
            Ok(turso_core::Value::Null)
        }
        napi::ValueType::Boolean => {
            let b = value.coerce_to_bool()?;
            Ok(turso_core::Value::Integer(b as i64))
        }
        napi::ValueType::Number => {
            let num = value.coerce_to_number()?.get_double()?;
            if num.fract() == 0.0 {
                Ok(turso_core::Value::Integer(num as i64))
            } else {
                Ok(turso_core::Value::Float(num))
            }
        }
        napi::ValueType::String => {
            let s = value.coerce_to_string()?;
            Ok(turso_core::Value::Text(s.into_utf8()?.as_str()?.into()))
        }
        napi::ValueType::Symbol
        | napi::ValueType::Object
        | napi::ValueType::Function
        | napi::ValueType::External => Err(napi::Error::new(
            napi::Status::GenericFailure,
            "Unsupported type",
        )),
    }
}

struct DatabaseFile {
    file: Arc<dyn turso_core::File>,
}

unsafe impl Send for DatabaseFile {}
unsafe impl Sync for DatabaseFile {}

impl DatabaseFile {
    pub fn new(file: Arc<dyn turso_core::File>) -> Self {
        Self { file }
    }
}

impl turso_core::DatabaseStorage for DatabaseFile {
    fn read_page(
        &self,
        page_idx: usize,
        c: turso_core::Completion,
    ) -> turso_core::Result<turso_core::Completion> {
        let r = c.as_read();
        let size = r.buf().len();
        assert!(page_idx > 0);
        if !(512..=65536).contains(&size) || size & (size - 1) != 0 {
            return Err(turso_core::LimboError::NotADB);
        }
        let pos = (page_idx - 1) * size;
        self.file.pread(pos, c)
    }

    fn write_page(
        &self,
        page_idx: usize,
        buffer: Arc<std::cell::RefCell<turso_core::Buffer>>,
        c: turso_core::Completion,
    ) -> turso_core::Result<turso_core::Completion> {
        let size = buffer.borrow().len();
        let pos = (page_idx - 1) * size;
        self.file.pwrite(pos, buffer, c)
    }
    fn write_pages(
        &self,
        page_idx: usize,
        page_size: usize,
        buffers: Vec<Arc<RefCell<turso_core::Buffer>>>,
        c: turso_core::Completion,
    ) -> turso_core::Result<turso_core::Completion> {
        let pos = page_idx.saturating_sub(1) * page_size;
        let c = self.file.pwritev(pos, buffers, c)?;
        Ok(c)
    }

    fn sync(&self, c: turso_core::Completion) -> turso_core::Result<turso_core::Completion> {
        self.file.sync(c)
    }

    fn size(&self) -> turso_core::Result<u64> {
        self.file.size()
    }
    fn truncate(
        &self,
        len: usize,
        c: turso_core::Completion,
    ) -> turso_core::Result<turso_core::Completion> {
        let c = self.file.truncate(len, c)?;
        Ok(c)
    }
}

#[inline]
fn into_napi_error(limbo_error: LimboError) -> napi::Error {
    napi::Error::new(napi::Status::GenericFailure, format!("{limbo_error}"))
}

#[inline]
fn into_napi_sqlite_error(limbo_error: LimboError) -> napi::Error<String> {
    napi::Error::new(String::from("SQLITE_ERROR"), format!("{limbo_error}"))
}

#[inline]
fn into_napi_error_with_message(
    error_code: String,
    limbo_error: LimboError,
) -> napi::Error<String> {
    napi::Error::new(error_code, format!("{limbo_error}"))
}

#[inline]
fn with_sqlite_error_message(err: napi::Error) -> napi::Error<String> {
    napi::Error::new("SQLITE_ERROR".to_owned(), err.reason.clone())
}

#[inline]
fn into_convertible_type_error_message(error_type: &str) -> String {
    "[TURSO_CONVERT_TYPE] ".to_owned() + error_type
}

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::Path,
    rc::Rc,
};

use rquickjs::{
    Array, Context, Ctx, Function, Object, Runtime, Value, function::Args, promise::PromiseState,
};

use crate::{
    error::{Error, Result},
    serde::{cbor_to_js, js_to_cbor_emit},
    transpile::strip_typescript,
    wasm::{future, isola::script::host::EmitType},
};

pub struct Scope {
    #[expect(
        dead_code,
        reason = "runtime must be kept alive for the context to function"
    )]
    runtime: Runtime,
    context: Context,
    rejections: Rc<RefCell<HashMap<u64, Rejection>>>,
}

#[derive(Clone)]
struct Rejection {
    cause: String,
    stack: Option<String>,
}

impl Rejection {
    fn from_value(value: &Value<'_>) -> Self {
        value.as_exception().map_or_else(
            || Self {
                cause: value.as_string().map_or_else(
                    || format!("{value:?}"),
                    |value| {
                        value
                            .to_string()
                            .unwrap_or_else(|_| "JavaScript error".to_string())
                    },
                ),
                stack: None,
            },
            |exception| Self {
                cause: exception
                    .message()
                    .unwrap_or_else(|| "Unhandled promise rejection".to_string()),
                stack: exception.stack(),
            },
        )
    }

    fn into_error(self) -> Error {
        Error::Js {
            cause: self.cause,
            stack: self.stack,
        }
    }
}

fn rejection_key(promise: &Value<'_>) -> u64 {
    let mut hasher = DefaultHasher::new();
    promise.hash(&mut hasher);
    hasher.finish()
}

fn is_boundary_cancellation(reason: &Value<'_>) -> bool {
    reason
        .as_object()
        .is_some_and(|reason| reason.get::<_, bool>("__isolaCancelled").unwrap_or(false))
}

pub enum InputValue<'a> {
    Cbor(Cow<'a, [u8]>),
    Iter(Vec<Vec<u8>>),
}

impl Scope {
    fn transpile(code: &str, source_name: Option<&Path>) -> Result<String> {
        strip_typescript(code, source_name).map_err(|err| Error::Transpile(err.to_string()))
    }

    fn input_to_js<'js>(ctx: &Ctx<'js>, input: InputValue<'_>) -> Result<Value<'js>> {
        match input {
            InputValue::Cbor(cbor) => cbor_to_js(ctx, cbor.as_ref()).map_err(|e| Error::Js {
                cause: e,
                stack: None,
            }),
            InputValue::Iter(items) => {
                let arr = Array::new(ctx.clone()).map_err(|_| Error::from_js_catch(ctx))?;
                for (index, item) in items.into_iter().enumerate() {
                    let value = cbor_to_js(ctx, &item).map_err(|e| Error::Js {
                        cause: e,
                        stack: None,
                    })?;
                    arr.set(index, value)
                        .map_err(|_| Error::from_js_catch(ctx))?;
                }
                Ok(arr.into_value())
            }
        }
    }

    pub fn new() -> Self {
        let runtime = Runtime::new().expect("failed to create QuickJS runtime");
        runtime.set_max_stack_size(2 * 1024 * 1024); // 2MB stack

        let rejections = Rc::new(RefCell::new(HashMap::new()));
        let tracked_rejections = Rc::clone(&rejections);
        runtime.set_host_promise_rejection_tracker(Some(Box::new(
            move |_ctx, promise, reason, is_handled| {
                let key = rejection_key(&promise);
                let mut rejections = tracked_rejections.borrow_mut();
                if is_handled || is_boundary_cancellation(&reason) {
                    rejections.remove(&key);
                } else {
                    rejections.insert(key, Rejection::from_value(&reason));
                }
            },
        )));

        let context = Context::full(&runtime).expect("failed to create QuickJS context");
        context.with(|ctx| {
            let globals = ctx.globals();
            // Compile boundary helpers once per QuickJS context. These are used
            // for every generator result and compiling them in the loop is costly.
            globals
                .set(
                    "__isola_async_iterator",
                    ctx.eval::<Function<'_>, _>(
                        "(function(o) { return typeof o[Symbol.asyncIterator] === 'function' ? o[Symbol.asyncIterator]() : null; })",
                    )
                    .expect("failed to initialize async iterator helper"),
                )
                .expect("failed to install async iterator helper");
            globals
                .set(
                    "__isola_iterator",
                    ctx.eval::<Function<'_>, _>(
                        "(function(o) { return typeof o[Symbol.iterator] === 'function' ? o[Symbol.iterator]() : null; })",
                    )
                    .expect("failed to initialize iterator helper"),
                )
                .expect("failed to install iterator helper");
            globals
                .set(
                    "__isola_next",
                    ctx.eval::<Function<'_>, _>("(function(g) { return g.next(); })")
                        .expect("failed to initialize next helper"),
                )
                .expect("failed to install next helper");
        });

        Self {
            runtime,
            context,
            rejections,
        }
    }

    pub const fn context(&self) -> &Context {
        &self.context
    }

    pub fn load_script(&self, code: &str) -> Result<()> {
        self.begin_boundary();
        let code = Self::transpile(code, None)?;
        let result = self.context.with(|ctx| {
            ctx.eval::<(), _>(code.as_str())
                .map_err(|_| Error::from_js_catch(&ctx))?;
            self.checkpoint(&ctx)
        });
        self.finish_boundary(result)
    }

    pub fn load_file(&self, path: &str) -> Result<()> {
        self.begin_boundary();
        let code = std::fs::read_to_string(path)
            .map_err(|_| Error::Unexpected("failed to read script"))?;
        let code = Self::transpile(&code, Some(Path::new(path)))?;
        let result = self.context.with(|ctx| {
            ctx.eval::<(), _>(code.as_str())
                .map_err(|_| Error::from_js_catch(&ctx))?;
            self.checkpoint(&ctx)
        });
        self.finish_boundary(result)
    }

    pub fn run<'a>(
        &self,
        name: &str,
        positional: impl IntoIterator<Item = InputValue<'a>>,
        named: impl IntoIterator<Item = (Cow<'a, str>, InputValue<'a>)>,
        mut callback: impl FnMut(EmitType, &[u8]),
    ) -> Result<()> {
        self.begin_boundary();
        let result = self.context.with(|ctx| {
            let globals = ctx.globals();
            let val: Value<'_> = globals.get(name).map_err(|_| Error::from_js_catch(&ctx))?;

            let obj = if val.is_function() {
                let func = val
                    .as_function()
                    .ok_or(Error::Unexpected("expected function"))?;

                // Build arguments
                let mut args: Vec<Value<'_>> = Vec::new();
                for v in positional {
                    args.push(Self::input_to_js(&ctx, v)?);
                }

                // Collect named args into a final options object
                let named_items: Vec<_> = named.into_iter().collect();
                if !named_items.is_empty() {
                    let opts = Object::new(ctx.clone()).map_err(|_| Error::from_js_catch(&ctx))?;
                    for (k, v) in named_items {
                        let val = Self::input_to_js(&ctx, v)?;
                        opts.set(k.as_ref(), val)
                            .map_err(|_| Error::from_js_catch(&ctx))?;
                    }
                    args.push(opts.into_value());
                }

                self.call_function(func, &ctx, args)?
            } else {
                val
            };

            self.checkpoint(&ctx)?;
            self.emit_result(&ctx, obj, &mut callback)
        });
        self.finish_boundary(result)
    }

    fn begin_boundary(&self) {
        self.rejections.borrow_mut().clear();
    }

    fn checkpoint(&self, ctx: &Ctx<'_>) -> Result<()> {
        while ctx.execute_pending_job() {
            if ctx.has_exception() {
                return Err(Error::from_js_catch(ctx));
            }
        }
        if ctx.has_exception() {
            return Err(Error::from_js_catch(ctx));
        }

        let mut rejections = self.rejections.borrow_mut();
        let rejection = rejections.values().next().cloned();
        rejections.clear();
        rejection.map_or(Ok(()), |rejection| Err(rejection.into_error()))
    }

    fn discard_jobs(&self, ctx: &Ctx<'_>) {
        while ctx.execute_pending_job() {
            if ctx.has_exception() {
                let _ = ctx.catch();
            }
        }
        if ctx.has_exception() {
            let _ = ctx.catch();
        }
        self.rejections.borrow_mut().clear();
    }

    fn finish_boundary<T>(&self, result: Result<T>) -> Result<T> {
        self.context.with(|ctx| {
            for _ in 0..8 {
                let cancelled = future::cancel_all(&ctx).unwrap_or_default();
                future::clear();
                self.discard_jobs(&ctx);
                if cancelled == 0 && !future::has_pending() {
                    break;
                }
            }
            let _ = future::discard_all(&ctx);
            future::clear();
        });
        self.rejections.borrow_mut().clear();
        result
    }

    /// Drive a Promise to completion using the runtime async handle loop.
    ///
    /// 1. Execute microtasks (`execute_pending_job`)
    /// 2. If promise unresolved and pending runtime ops exist, resolve ready
    ///    handles
    /// 3. Call JS `_isola_async._resolve(readyHandles)` to resolve
    ///    corresponding Promises
    /// 4. New microtasks are created → repeat
    fn drive_promise<'js>(
        &self,
        ctx: &Ctx<'js>,
        promise: &rquickjs::Promise<'js>,
        suspend: bool,
    ) -> Result<Value<'js>> {
        loop {
            // Promise callbacks can settle an already-resolved chain or expose
            // a detached rejection before any runtime operation is polled.
            self.checkpoint(ctx)?;

            match promise.result::<Value<'js>>() {
                Some(Ok(val)) => return Ok(val),
                Some(Err(_)) => return Err(Error::from_js_catch(ctx)),
                None => {}
            }

            if !future::has_pending() {
                return Err(Error::Unexpected("promise never resolved"));
            }

            let mut drive_error = None;
            let made_progress = future::drive_pending(|ready_handles| {
                if !ready_handles.is_empty()
                    && let Err(_error) = future::resolve_ready(ctx, ready_handles)
                {
                    drive_error = Some(Error::from_js_catch(ctx));
                    return future::Drive::Stop;
                }
                if let Err(error) = self.checkpoint(ctx) {
                    drive_error = Some(error);
                    return future::Drive::Stop;
                }
                if promise.state() == PromiseState::Pending {
                    future::Drive::Wait
                } else if suspend {
                    future::Drive::Suspend
                } else {
                    future::Drive::Stop
                }
            });
            if let Some(error) = drive_error {
                return Err(error);
            }
            if !made_progress && promise.state() == PromiseState::Pending {
                return Err(Error::Unexpected("promise never resolved"));
            }
        }
    }

    fn call_function<'js>(
        &self,
        func: &Function<'js>,
        ctx: &Ctx<'js>,
        args: Vec<Value<'js>>,
    ) -> Result<Value<'js>> {
        let mut js_args = Args::new(ctx.clone(), args.len());
        for arg in args {
            js_args
                .push_arg(arg)
                .map_err(|_| Error::from_js_catch(ctx))?;
        }
        let result: Value<'js> = func
            .call_arg(js_args)
            .map_err(|_| Error::from_js_catch(ctx))?;

        self.checkpoint(ctx)?;

        if result.is_promise() {
            let promise = result
                .as_promise()
                .ok_or(Error::Unexpected("expected promise"))?;

            self.drive_promise(ctx, promise, false)
        } else {
            Ok(result)
        }
    }

    fn emit_result<'js>(
        &self,
        ctx: &Ctx<'js>,
        obj: Value<'js>,
        callback: &mut impl FnMut(EmitType, &[u8]),
    ) -> Result<()> {
        // Check if it's an async generator (has Symbol.asyncIterator) BEFORE sync
        // generator check, because async generators also have .next() but
        // return Promises.
        if let Some(gen_obj) = obj.as_object() {
            let check_async_iter: std::result::Result<Function<'_>, _> =
                ctx.globals().get("__isola_async_iterator");
            if let Ok(check_fn) = check_async_iter
                && let Ok(iter) = check_fn.call::<_, Value<'_>>((gen_obj.clone(),))
                && iter.is_object()
                && !iter.is_null()
            {
                return self.iterate_async_generator(ctx, &iter, callback);
            }
        }

        // Check if it's a sync generator (has next() method)
        if let Some(gen_obj) = obj.as_object()
            && gen_obj.get::<_, Function<'_>>("next").is_ok()
        {
            // Use a JS helper to call next() with proper `this` binding
            let call_next: Function<'_> = ctx
                .globals()
                .get("__isola_next")
                .map_err(|_| Error::from_js_catch(ctx))?;

            // It's a generator - iterate it
            loop {
                let iter_result: Object<'_> = call_next
                    .call((gen_obj.clone(),))
                    .map_err(|_| Error::from_js_catch(ctx))?;
                self.checkpoint(ctx)?;
                let done: bool = iter_result.get("done").unwrap_or(false);
                let value: Value<'_> = iter_result
                    .get("value")
                    .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));

                if done {
                    // Final value from generator - if not undefined, emit as end
                    if value.is_undefined() {
                        callback(EmitType::End, &[]);
                    } else {
                        js_to_cbor_emit(value, EmitType::End, callback).map_err(|e| Error::Js {
                            cause: e,
                            stack: None,
                        })?;
                    }
                    return Ok(());
                }

                js_to_cbor_emit(value, EmitType::PartialResult, &mut *callback).map_err(|e| {
                    Error::Js {
                        cause: e,
                        stack: None,
                    }
                })?;
            }
        }

        // Regular serializable value
        if Self::is_serializable(&obj) {
            return js_to_cbor_emit(obj, EmitType::End, callback).map_err(|e| Error::Js {
                cause: e,
                stack: None,
            });
        }

        // Try Symbol.iterator
        if let Some(iter_obj) = obj.as_object() {
            let get_iter: std::result::Result<Function<'_>, _> =
                ctx.globals().get("__isola_iterator");
            if let Ok(get_iter) = get_iter
                && let Ok(iter) = get_iter.call::<_, Value<'_>>((iter_obj.clone(),))
                && let Some(iter_val) = iter.as_object()
                && iter_val.get::<_, Function<'_>>("next").is_ok()
            {
                let call_next: Function<'_> = ctx
                    .globals()
                    .get("__isola_next")
                    .map_err(|_| Error::from_js_catch(ctx))?;
                loop {
                    let iter_result: Object<'_> = call_next
                        .call((iter_val.clone(),))
                        .map_err(|_| Error::from_js_catch(ctx))?;
                    self.checkpoint(ctx)?;
                    let done: bool = iter_result.get("done").unwrap_or(false);
                    if done {
                        callback(EmitType::End, &[]);
                        return Ok(());
                    }
                    let value: Value<'_> = iter_result
                        .get("value")
                        .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
                    js_to_cbor_emit(value, EmitType::PartialResult, &mut *callback).map_err(
                        |e| Error::Js {
                            cause: e,
                            stack: None,
                        },
                    )?;
                }
            }
        }

        Err(Error::Unexpected(
            "Return type is not serializable or iterable",
        ))
    }

    /// Iterate an async generator to completion, emitting each yielded value.
    fn iterate_async_generator<'js>(
        &self,
        ctx: &Ctx<'js>,
        iter: &Value<'js>,
        callback: &mut impl FnMut(EmitType, &[u8]),
    ) -> Result<()> {
        let call_next: Function<'_> = ctx
            .globals()
            .get("__isola_next")
            .map_err(|_| Error::from_js_catch(ctx))?;

        loop {
            // Each call to next() returns a Promise<{value, done}>
            let next_result: Value<'_> = call_next
                .call((iter.clone(),))
                .map_err(|_| Error::from_js_catch(ctx))?;
            self.checkpoint(ctx)?;

            // Drive the promise to completion
            let iter_result = if next_result.is_promise() {
                let promise = next_result
                    .as_promise()
                    .ok_or(Error::Unexpected("expected promise from async generator"))?;
                self.drive_promise(ctx, promise, true)?
            } else {
                next_result
            };

            let iter_obj: Object<'_> = iter_result
                .as_object()
                .cloned()
                .ok_or(Error::Unexpected("expected object from next()"))?;
            let done: bool = iter_obj.get("done").unwrap_or(false);
            let value: Value<'_> = iter_obj
                .get("value")
                .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));

            if done {
                if value.is_undefined() {
                    callback(EmitType::End, &[]);
                } else {
                    js_to_cbor_emit(value, EmitType::End, callback).map_err(|e| Error::Js {
                        cause: e,
                        stack: None,
                    })?;
                }
                return Ok(());
            }

            if let Err(cause) = js_to_cbor_emit(value, EmitType::PartialResult, &mut *callback) {
                let _ = self.close_async_iterator(ctx, iter);
                return Err(Error::Js { cause, stack: None });
            }
        }
    }

    fn close_async_iterator<'js>(&self, ctx: &Ctx<'js>, iter: &Value<'js>) -> Result<()> {
        let close: Function<'_> = ctx
            .eval(
                "(function(g) { return typeof g.return === 'function' ? g.return.call(g) : undefined; })",
            )
            .map_err(|_| Error::from_js_catch(ctx))?;
        let result: Value<'_> = close
            .call((iter.clone(),))
            .map_err(|_| Error::from_js_catch(ctx))?;

        if let Some(promise) = result.as_promise() {
            let _ = self.drive_promise(ctx, promise, false)?;
        } else {
            self.checkpoint(ctx)?;
        }
        Ok(())
    }

    fn is_serializable(val: &Value<'_>) -> bool {
        val.is_null()
            || val.is_undefined()
            || val.is_bool()
            || val.is_int()
            || val.is_number()
            || val.is_string()
            || val.is_array()
            || val.is_object()
    }
}

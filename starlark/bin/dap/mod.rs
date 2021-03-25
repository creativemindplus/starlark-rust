/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::eval::{dialect, globals, Context};
use debugserver_types::*;
use gazebo::prelude::*;
pub use library::*;
use serde_json::{Map, Value};
use starlark::{
    codemap::{Span, SpanLoc},
    debug,
    environment::Module,
    eval::Evaluator,
    syntax::AstModule,
};
use std::{
    collections::{HashMap, HashSet},
    mem,
    path::{Path, PathBuf},
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
};

mod library;

#[derive(Debug)]
struct Backend {
    client: Client,
    starlark: Context,

    file: Mutex<Option<String>>,

    // These breakpoints must all match statements as per on_stmt.
    // Those values for which we abort the execution.
    breakpoints: Arc<Mutex<HashMap<String, HashSet<Span>>>>,

    sender: Sender<Box<dyn Fn(Span, &mut Evaluator) -> Next + Send>>,
    receiver: Arc<Mutex<Receiver<Box<dyn Fn(Span, &mut Evaluator) -> Next + Send>>>>,
}

enum Next {
    Continue,
    RemainPaused,
}

impl Backend {
    fn inject<T: 'static + Send>(
        &self,
        f: Box<dyn Fn(Span, &mut Evaluator) -> (Next, T) + Send>,
    ) -> T {
        let (sender, receiver) = channel();
        self.sender
            .send(box move |span, ctx| {
                let (next, res) = f(span, ctx);
                sender.send(res).unwrap();
                next
            })
            .unwrap();
        receiver.recv().unwrap()
    }

    fn inject_continue(&self) {
        self.inject(box |_, _| (Next::Continue, ()))
    }

    fn with_ctx<T: 'static + Send>(&self, f: Box<dyn Fn(Span, &mut Evaluator) -> T + Send>) -> T {
        self.inject(box move |span, ctx| (Next::RemainPaused, f(span, ctx)))
    }

    fn execute(&self, path: &str) {
        let client = self.client.dupe();
        let client2 = self.client.dupe();
        let path = PathBuf::from(path);
        let breakpoints = self.breakpoints.dupe();
        let receiver = self.receiver.dupe();

        let go = move || -> anyhow::Result<String> {
            client.log(&format!("EVALUATION PREPARE: {}", path.display()));
            let ast = AstModule::parse_file(&path, &dialect())?;
            let module = Module::new();
            let globals = globals();
            let mut ctx = Evaluator::new(&module, &globals);
            let fun = |span, ctx: &mut Evaluator| {
                let stop = {
                    let breaks = breakpoints.lock().unwrap();
                    let span_loc = ctx.look_up_span(span);
                    breaks
                        .get(span_loc.file.name())
                        .map(|set| set.contains(&span))
                        .unwrap_or_default()
                };
                if stop {
                    client.event_stopped(StoppedEventBody {
                        reason: "breakpoint".to_owned(),
                        thread_id: Some(0),
                        description: Some("Hello".to_owned()),
                        all_threads_stopped: Some(true),
                        preserve_focus_hint: None,
                        text: None,
                    });
                    loop {
                        let msg = receiver.lock().unwrap().recv().unwrap();
                        match msg(span, ctx) {
                            Next::Continue => break,
                            Next::RemainPaused => continue,
                        }
                    }
                }
            };
            ctx.on_stmt = Some(&fun);
            // No way to pass back success/failure to the caller
            client.log(&format!("EVALUATION START: {}", path.display()));
            let v = ctx.eval_module(ast)?;
            let s = v.to_string();
            client.log(&format!("EVALUATION FINISHED: {}", path.display()));
            Ok(s)
        };

        thread::spawn(move || {
            let res = go();
            let output = match &res {
                Err(e) => format!("{:#}", e),
                Ok(v) => v.to_owned(),
            };
            client2.event_output(OutputEventBody {
                output,
                category: None,
                column: None,
                data: None,
                line: None,
                source: None,
                variables_reference: None,
            });
            client2.event_exited(ExitedEventBody {
                exit_code: if res.is_ok() { 0 } else { 1 },
            });
            client2.event_terminated(None);
        });
    }
}

fn breakpoint(verified: bool) -> Breakpoint {
    Breakpoint {
        column: None,
        end_column: None,
        end_line: None,
        id: None,
        line: None,
        message: None,
        source: None,
        verified,
    }
}

impl DebugServer for Backend {
    fn initialize(&self, _: InitializeRequestArguments) -> anyhow::Result<Option<Capabilities>> {
        self.client.event_initialized(None);
        Ok(Some(Capabilities {
            supports_configuration_done_request: Some(true),
            supports_evaluate_for_hovers: Some(true),
            supports_set_variable: Some(true),
            supports_step_in_targets_request: Some(true),
            ..Capabilities::default()
        }))
    }

    fn set_breakpoints(
        &self,
        x: SetBreakpointsArguments,
    ) -> anyhow::Result<SetBreakpointsResponseBody> {
        let breakpoints = x.breakpoints.unwrap_or_default();
        let source = x.source.path.unwrap();

        if breakpoints.is_empty() {
            self.breakpoints.lock().unwrap().remove(&source);
            Ok(SetBreakpointsResponseBody {
                breakpoints: Vec::new(),
            })
        } else {
            match AstModule::parse_file(Path::new(&source), &dialect()) {
                Err(_) => {
                    self.breakpoints.lock().unwrap().remove(&source);
                    Ok(SetBreakpointsResponseBody {
                        breakpoints: vec![breakpoint(false); breakpoints.len()],
                    })
                }
                Ok(ast) => {
                    let poss: HashMap<usize, Span> = debug::stmt_locations(&ast)
                        .iter()
                        .map(|x| {
                            let span = ast.look_up_span(*x);
                            (span.begin.line, *x)
                        })
                        .collect();
                    let list = breakpoints.map(|x| poss.get(&(x.line as usize - 1)));
                    self.breakpoints
                        .lock()
                        .unwrap()
                        .insert(source, list.iter().filter_map(|x| x.copied()).collect());
                    Ok(SetBreakpointsResponseBody {
                        breakpoints: list.map(|x| breakpoint(x.is_some())),
                    })
                }
            }
        }
    }

    fn set_exception_breakpoints(&self, _: SetExceptionBreakpointsArguments) -> anyhow::Result<()> {
        // We just assume that break on error is always useful
        Ok(())
    }

    fn launch(&self, _: LaunchRequestArguments, args: Map<String, Value>) -> anyhow::Result<()> {
        // Expecting program of type string
        match args.get("program") {
            Some(Value::String(path)) => {
                *self.file.lock().unwrap() = Some(path.to_owned());
                Ok(())
            }
            _ => Err(anyhow::anyhow!(
                "Couldn't find a program to launch, got args {:?}",
                args
            )),
        }
    }

    fn threads(&self) -> anyhow::Result<ThreadsResponseBody> {
        Ok(ThreadsResponseBody {
            threads: vec![Thread {
                id: 0,
                name: "main".to_string(),
            }],
        })
    }

    fn configuration_done(&self) -> anyhow::Result<()> {
        if let Some(path) = self.file.lock().unwrap().as_ref() {
            self.execute(path);
        }
        Ok(())
    }

    fn stack_trace(&self, _: StackTraceArguments) -> anyhow::Result<StackTraceResponseBody> {
        fn convert_frame(id: usize, name: String, location: Option<SpanLoc>) -> StackFrame {
            let mut s = StackFrame {
                id: id as i64,
                name,
                column: 0,
                line: 0,
                end_column: None,
                end_line: None,
                module_id: None,
                presentation_hint: None,
                source: None,
            };
            if let Some(loc) = location {
                s.line = loc.begin.line as i64 + 1;
                s.column = loc.begin.column as i64 + 1;
                s.end_line = Some(loc.end.line as i64 + 1);
                s.end_column = Some(loc.end.column as i64 + 1);
                s.source = Some(Source {
                    path: Some(loc.file.name().to_owned()),
                    ..Source::default()
                })
            }
            s
        }

        // Our model of a Frame and the debugger model are a bit different.
        // We record the location of the call, but DAP wants the location we are at.
        // We also have them in the wrong order
        self.with_ctx(box |span, ctx| {
            let frames = ctx.call_stack().to_diagnostic_frames();
            let mut next = Some(ctx.look_up_span(span));
            let mut res = Vec::with_capacity(frames.len() + 1);
            for (i, x) in frames.iter().rev().enumerate() {
                res.push(convert_frame(i, x.name.clone(), next));
                next = x.location.clone();
            }
            res.push(convert_frame(10000, "Root".to_owned(), next));
            Ok(StackTraceResponseBody {
                total_frames: Some(res.len() as i64),
                stack_frames: res,
            })
        })
    }

    fn scopes(&self, _: ScopesArguments) -> anyhow::Result<ScopesResponseBody> {
        self.with_ctx(box |_, ctx| {
            let vars = debug::inspect_variables(ctx);
            Ok(ScopesResponseBody {
                scopes: vec![Scope {
                    name: "Locals".to_owned(),
                    named_variables: Some(vars.len() as i64),
                    variables_reference: 2000,
                    expensive: false,
                    column: None,
                    end_column: None,
                    end_line: None,
                    indexed_variables: None,
                    line: None,
                    source: None,
                }],
            })
        })
    }

    fn variables(&self, _: VariablesArguments) -> anyhow::Result<VariablesResponseBody> {
        self.with_ctx(box |_, ctx| {
            let vars = debug::inspect_variables(ctx);
            Ok(VariablesResponseBody {
                variables: vars
                    .into_iter()
                    .map(|(name, value)| Variable {
                        name,
                        value: value.to_string(),
                        type_: Some(value.get_type().to_owned()),
                        evaluate_name: None,
                        indexed_variables: None,
                        named_variables: None,
                        presentation_hint: None,
                        variables_reference: 0,
                    })
                    .collect(),
            })
        })
    }

    fn continue_(&self, _: ContinueArguments) -> anyhow::Result<ContinueResponseBody> {
        self.inject_continue();
        Ok(ContinueResponseBody::default())
    }

    fn evaluate(&self, x: EvaluateArguments) -> anyhow::Result<EvaluateResponseBody> {
        self.with_ctx(box move |_, ctx| {
            // We don't want to trigger breakpoints during an evaluate,
            // not least because we currently don't allow reenterant evaluate
            let old = mem::take(&mut ctx.on_stmt);
            let s = match debug::evaluate(x.expression.clone(), ctx) {
                Err(e) => format!("{:#}", e),
                Ok(v) => v.to_string(),
            };
            ctx.on_stmt = old;
            Ok(EvaluateResponseBody {
                indexed_variables: None,
                named_variables: None,
                presentation_hint: None,
                result: s,
                type_: None,
                variables_reference: 0.0,
            })
        })
    }
}

pub fn server(starlark: Context) {
    let (sender, receiver) = channel();
    DapService::run(|client| Backend {
        client,
        starlark,
        breakpoints: Default::default(),
        file: Default::default(),
        sender,
        receiver: Arc::new(Mutex::new(receiver)),
    })
}

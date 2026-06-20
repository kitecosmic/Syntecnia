//! A1 — Concurrencia, Fase 1. Implementa `parallel_map` + `chunk` (ver
//! SPEC-CONCURRENCIA.md). Es feature NUEVA (no está en el oráculo Python).
//!
//! `parallel_map(task, list, limit?)`: aplica `task` a cada item concurrentemente
//! sobre un **pool de hilos acotado** (`limit`, default 64), cada hilo con su propio
//! intérprete sync (modelo CSP/SendValue, como `spawn`) que hereda las capabilities
//! del scope. Resultados **en orden de entrada**; **fail-fast** (el primer error —
//! el de menor índice, como `apply` secuencial — cancela el resto y propaga).
//! `chunk(list, size)`: parte una lista en sublistas de `size`.
//!
//! El intérprete sigue síncrono; la concurrencia se agrega alrededor (std::thread).
//! rayon (cómputo puro) y tokio (C100k, Fase 2) quedan diferidos. Coordinación
//! cross-worker por blackboard no está en Fase 1 (cada worker aislado).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use syntecnia_capabilities::model::{Capability, CapabilitySet};
use syntecnia_core::ast::Node;
use syntecnia_core::interpreter::{Control, Interpreter, RuntimeError};
use syntecnia_core::number::Number;
use syntecnia_core::types::{from_send, syn_list, to_send, SendValue, SynTaskValue, SynValue};

use crate::engine::{wire_common, INTERP_STACK_SIZE};
use crate::serve::{rebuild_globals, snapshot_globals, GlobalVal};

fn err(msg: &str) -> Control {
    Control::Error(RuntimeError::new(msg.to_string()))
}

fn num_i64(n: &Number) -> i64 {
    match n {
        Number::Int(i) => *i,
        Number::Float(f) => *f as i64,
        Number::Big(b) => b.to_string().parse().unwrap_or(0),
    }
}

/// Convierte un `Control` de error en `RuntimeError` (preservando la ubicación del
/// worker, para que el error de `parallel_map` sea idéntico al de `apply` secuencial).
fn control_to_error(c: Control) -> RuntimeError {
    match c {
        Control::Error(e) => e,
        Control::Give(_) => RuntimeError::new("'give' used outside of a task".to_string()),
        Control::Stop(_) => RuntimeError::new("'stop' used outside of a loop".to_string()),
    }
}

/// Snapshot `Send` del task a aplicar (task de usuario con su AST, o un builtin por nombre).
enum TaskSnapshot {
    User {
        name: String,
        parameters: Vec<String>,
        body: Vec<Node>,
        required_capabilities: Vec<(String, Option<String>)>,
    },
    Builtin(String),
}

/// Intérprete worker fresco: builtins + caps heredadas + globales reconstruidos.
fn build_worker_interp(
    globals: &[(String, GlobalVal)],
    granted: &[Capability],
    denied: &[Capability],
    secure: bool,
) -> Interpreter {
    let mut interp = Interpreter::new();
    let caps = Rc::new(RefCell::new(CapabilitySet::new("parallel")));
    {
        let mut c = caps.borrow_mut();
        for cap in granted {
            c.grant(cap.clone());
        }
        for cap in denied {
            c.deny(cap.clone());
        }
    }
    wire_common(&mut interp, &caps, secure);
    rebuild_globals(&interp, globals);
    interp.freeze_intent(); // corre bajo el intent congelado
    interp
}

fn reconstruct_task(interp: &Interpreter, snap: &TaskSnapshot) -> SynValue {
    match snap {
        TaskSnapshot::User { name, parameters, body, required_capabilities } => {
            SynValue::Task(Rc::new(SynTaskValue {
                name: name.clone(),
                parameters: parameters.clone(),
                body: body.clone(),
                closure_env: interp.global_env.clone(),
                origin: None,
                required_capabilities: required_capabilities.clone(),
            }))
        }
        TaskSnapshot::Builtin(name) => {
            interp.global_env.borrow().bindings.get(name).cloned().unwrap_or(SynValue::Nothing)
        }
    }
}

/// Corre `task` sobre `items` con un pool de `limit` hilos. Resultados en orden;
/// fail-fast con el error de menor índice.
#[allow(clippy::too_many_arguments)]
fn run_parallel(
    globals: Arc<Vec<(String, GlobalVal)>>,
    granted: Arc<Vec<Capability>>,
    denied: Arc<Vec<Capability>>,
    task_snap: Arc<TaskSnapshot>,
    items: Vec<SendValue>,
    limit: usize,
    secure: bool,
) -> Result<Vec<SendValue>, RuntimeError> {
    let n = items.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let workers = limit.max(1).min(n);
    let items = Arc::new(items);
    let results: Arc<Vec<Mutex<Option<SendValue>>>> =
        Arc::new((0..n).map(|_| Mutex::new(None)).collect());
    let next = Arc::new(AtomicUsize::new(0));
    let aborted = Arc::new(AtomicBool::new(false));
    let error: Arc<Mutex<Option<(usize, RuntimeError)>>> = Arc::new(Mutex::new(None));

    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let globals = globals.clone();
        let granted = granted.clone();
        let denied = denied.clone();
        let task_snap = task_snap.clone();
        let items = items.clone();
        let results = results.clone();
        let next = next.clone();
        let aborted = aborted.clone();
        let error = error.clone();
        let handle = std::thread::Builder::new()
            .stack_size(INTERP_STACK_SIZE)
            .spawn(move || {
                let mut interp = build_worker_interp(&globals, &granted, &denied, secure);
                let task_value = reconstruct_task(&interp, &task_snap);
                loop {
                    if aborted.load(Ordering::Relaxed) {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= items.len() {
                        break;
                    }
                    let item = from_send(&items[i]);
                    match interp.call_task(task_value.clone(), vec![item]) {
                        Ok(v) => {
                            *results[i].lock().unwrap() = Some(to_send(&v));
                        }
                        Err(c) => {
                            let re = control_to_error(c);
                            let mut e = error.lock().unwrap();
                            if e.as_ref().map_or(true, |(idx, _)| i < *idx) {
                                *e = Some((i, re));
                            }
                            aborted.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            })
            .expect("no se pudo crear el hilo worker");
        handles.push(handle);
    }
    for h in handles {
        let _ = h.join();
    }

    if let Some((_, re)) = error.lock().unwrap().take() {
        return Err(re);
    }
    let out = results
        .iter()
        .map(|m| m.lock().unwrap().clone().unwrap_or(SendValue::Nothing))
        .collect();
    Ok(out)
}

/// Registra `parallel_map` y `chunk` (lo llama `wire_common`).
pub fn register_parallel_builtins(interp: &Interpreter, caps: &Rc<RefCell<CapabilitySet>>, secure: bool) {
    // chunk(list, size) — parte una lista en sublistas de `size`. Puro.
    interp.register_builtin(
        "chunk",
        2,
        Rc::new(|_i, args, _loc| {
            let list = match args.first() {
                Some(SynValue::List(l)) => l.borrow().clone(),
                _ => return Err(err("chunk: first argument must be a list")),
            };
            let size = match args.get(1) {
                Some(SynValue::Number(n)) => num_i64(n),
                _ => return Err(err("chunk: size must be a number")),
            };
            if size <= 0 {
                return Err(err("chunk size must be positive"));
            }
            let size = size as usize;
            let chunks: Vec<SynValue> = list.chunks(size).map(|c| syn_list(c.to_vec())).collect();
            Ok(syn_list(chunks))
        }),
    );

    // parallel_map(task, list, limit?) — fan-out acotado, resultados en orden.
    let caps = caps.clone();
    interp.register_builtin(
        "parallel_map",
        -1,
        Rc::new(move |i, args, _loc| {
            let task = match args.first() {
                Some(t @ (SynValue::Task(_) | SynValue::Builtin(_))) => t,
                _ => return Err(err("parallel_map: first argument must be a task")),
            };
            let list = match args.get(1) {
                Some(SynValue::List(l)) => l.borrow().clone(),
                _ => return Err(err("parallel_map: second argument must be a list")),
            };
            let limit = match args.get(2) {
                Some(SynValue::Number(n)) => {
                    let v = num_i64(n);
                    if v <= 0 {
                        64
                    } else {
                        v as usize
                    }
                }
                _ => 64,
            };
            let task_snap = match task {
                SynValue::Task(t) => TaskSnapshot::User {
                    name: t.name.clone(),
                    parameters: t.parameters.clone(),
                    body: t.body.clone(),
                    required_capabilities: t.required_capabilities.clone(),
                },
                SynValue::Builtin(b) => TaskSnapshot::Builtin(b.name.clone()),
                _ => unreachable!(),
            };
            let globals = snapshot_globals(i);
            let granted: Vec<Capability> = caps.borrow().granted.iter().cloned().collect();
            let denied: Vec<Capability> = caps.borrow().denied.iter().cloned().collect();
            let items: Vec<SendValue> = list.iter().map(to_send).collect();
            match run_parallel(
                globals,
                Arc::new(granted),
                Arc::new(denied),
                Arc::new(task_snap),
                items,
                limit,
                secure,
            ) {
                Ok(results) => Ok(syn_list(results.iter().map(from_send).collect())),
                Err(re) => Err(Control::Error(re)),
            }
        }),
    );
}

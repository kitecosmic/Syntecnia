//! Servidor HTTP nativo. Port de `syntecnia/stdlib/server.py`.
//!
//! Implementa el constructo `serve on PORT`. La lógica del lenguaje (capability,
//! aislamiento por request, auth, validación) la pone el engine vía closures
//! inyectados; este módulo es el plumbing HTTP + el *contrato de respuesta*.
//!
//! Concurrencia: thread-per-connection con `std::thread` (paridad con
//! `ThreadingHTTPServer`). Los recursos compartidos (db/blackboard) se envuelven
//! en `Arc`/`Mutex` antes de wirearse a los handlers.
//!
//! Este archivo cubre, por subsistemas (cada uno gateado con su differential):
//!   1. routing/match  — `specificity`, `path_match`, `match_route`, `methods_for_path`
//!   2. contrato de respuesta — pendiente (envelopes/paginación)
//!   ... (static, negotiation, rate-limit, SSE, discovery, CORS, max_body)

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use indexmap::IndexMap;

use syntecnia_core::interpreter::Interpreter;
use syntecnia_core::number::{py_float_str, Number};
use syntecnia_core::types::{
    syn_bool, syn_int, syn_list, syn_map, syn_nothing, syn_text, ServerValue, SynValue,
};

// =========================================================
// Constantes
// =========================================================

pub const DEFAULT_LIMIT: i64 = 100;
pub const MAX_LIMIT: i64 = 1000;
/// Tope por defecto del body bufferizado en memoria (no es tope duro: `max_body`
/// lo overridea y los bodies grandes spillean a disco).
pub const MAX_BODY: i64 = 1_048_576;
/// Sobre este tamaño el body se streamea a un temp file en vez de memoria.
pub const MEM_SPILL: usize = 1_048_576;
/// Tope por defecto de streams SSE concurrentes.
pub const DEFAULT_MAX_STREAMS: i64 = 100;

/// Content-types pinneados para servir estáticos: el resultado nunca depende del
/// registro de mimetypes del host (p.ej. Windows mapea .js → text/plain).
pub fn web_content_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        ".html" | ".htm" => "text/html; charset=utf-8",
        ".css" => "text/css; charset=utf-8",
        ".js" | ".mjs" => "text/javascript; charset=utf-8",
        ".json" | ".map" => "application/json; charset=utf-8",
        ".svg" => "image/svg+xml",
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".webp" => "image/webp",
        ".ico" => "image/x-icon",
        ".woff" => "font/woff",
        ".woff2" => "font/woff2",
        ".ttf" => "font/ttf",
        ".txt" => "text/plain; charset=utf-8",
        ".xml" => "application/xml; charset=utf-8",
        ".wasm" => "application/wasm",
        ".pdf" => "application/pdf",
        _ => return None,
    })
}

/// Una respuesta escrita verbatim (no JSON), con Content-Type explícito.
/// Producida por html()/respond()/render() y por el servido de estáticos.
#[derive(Clone, Debug)]
pub struct RawResponse {
    pub body: Vec<u8>,
    pub content_type: String,
    pub status: u16,
}

impl RawResponse {
    pub fn text(body: impl Into<String>, content_type: impl Into<String>, status: u16) -> Self {
        RawResponse { body: body.into().into_bytes(), content_type: content_type.into(), status }
    }
}

/// Resultado de servir un estático de producción: status + body + content-type +
/// headers extra (ETag, Accept-Ranges, Content-Range, Content-Encoding, Vary).
struct StaticResp {
    status: u16,
    body: Vec<u8>,
    content_type: String,
    extra: Vec<(String, String)>,
}

/// ETag débil-equivalente derivado de tamaño + mtime (como hace nginx por defecto).
fn etag_for(path: &Path, size: usize) -> String {
    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", size, mtime)
}

/// Tipos comprimibles con gzip (html/css/js/json/svg/txt + xml por extensión común).
fn is_compressible(content_type: &str) -> bool {
    let c = content_type.split(';').next().unwrap_or("").trim();
    matches!(
        c,
        "text/html"
            | "text/css"
            | "text/javascript"
            | "application/javascript"
            | "application/json"
            | "image/svg+xml"
            | "text/plain"
            | "application/xml"
            | "text/xml"
    )
}

/// Comprime con gzip (flate2 / miniz_oxide puro-Rust). None si falla.
fn gzip_bytes(data: &[u8]) -> Option<Vec<u8>> {
    use flate2::{write::GzEncoder, Compression};
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(data).ok()?;
    e.finish().ok()
}

/// Parsea un header `Range: bytes=START-END` (un solo rango). Soporta sufijo
/// `bytes=-N`. Devuelve `(start, end)` inclusivos, o None si el rango es inválido.
fn parse_range(range: &str, size: usize) -> Option<(usize, usize)> {
    if size == 0 {
        return None;
    }
    let r = range.trim().strip_prefix("bytes=")?;
    let (s, e) = r.split_once('-')?;
    let (s, e) = (s.trim(), e.trim());
    let (start, end) = if s.is_empty() {
        // Sufijo: últimos N bytes.
        let n: usize = e.parse().ok()?;
        if n == 0 {
            return None;
        }
        (size.saturating_sub(n), size - 1)
    } else {
        let start: usize = s.parse().ok()?;
        let end = if e.is_empty() {
            size - 1
        } else {
            e.parse::<usize>().ok()?.min(size - 1)
        };
        (start, end)
    };
    if start > end || start >= size {
        return None;
    }
    Some((start, end))
}

// =========================================================
// URL helpers (unquote, query parse)
// =========================================================

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decodifica percent-encoding (`%XX`) como `urllib.parse.unquote` (UTF-8, lossy).
/// No toca `+` (eso es `unquote_plus`).
pub fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Separa una URL en (path, query-map). Espeja `urlparse` + `parse_qs` con
/// `{k: v[-1]}` (último valor gana para claves repetidas).
pub fn parse_path_query(raw: &str) -> (String, IndexMap<String, String>) {
    let (path_part, query_part) = match raw.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw, ""),
    };
    // urlparse separa también el fragmento (#...); el path es hasta '?' o '#'.
    let path_part = path_part.split('#').next().unwrap_or(path_part);
    let mut query: IndexMap<String, String> = IndexMap::new();
    if !query_part.is_empty() {
        for pair in query_part.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            // parse_qs ignora claves vacías y reemplaza '+' por espacio.
            let key = unquote(&k.replace('+', " "));
            if key.is_empty() {
                continue;
            }
            let val = unquote(&v.replace('+', " "));
            query.insert(key, val); // último gana (parse_qs v[-1])
        }
    }
    (path_part.to_string(), query)
}

// =========================================================
// Routing / match (subsistema 1)
// =========================================================

/// Segmentos no vacíos de un path (split por '/').
fn segments(pattern: &str) -> Vec<&str> {
    pattern.split('/').filter(|s| !s.is_empty()).collect()
}

/// Rango de especificidad de cada segmento: estático(0) < :param(1) < *catchall(2).
/// Ordenar rutas por esta lista ascendente pone la más específica primero.
pub fn specificity(pattern: &str) -> Vec<i32> {
    segments(pattern)
        .iter()
        .map(|seg| {
            if seg.starts_with('*') {
                2
            } else if seg.starts_with(':') {
                1
            } else {
                0
            }
        })
        .collect()
}

/// True si el último segmento del patrón es un `:param` (puede tragarse un sufijo
/// de formato). Un literal o `*catchall` mantiene el valor con punto.
pub fn param_last_segment(pattern: &str) -> bool {
    segments(pattern).last().map_or(false, |s| s.starts_with(':'))
}

/// Devuelve los params capturados si `path` matchea `pattern`, o None.
/// Un `*name` es catch-all: debe ir último y captura el resto (≥1 segmento).
pub fn path_match(pattern: &str, path: &str) -> Option<IndexMap<String, String>> {
    let actual = segments(path);
    let segs = segments(pattern);
    let mut params: IndexMap<String, String> = IndexMap::new();
    for (i, pat_seg) in segs.iter().enumerate() {
        if let Some(name) = pat_seg.strip_prefix('*') {
            let rest = &actual[i..];
            if rest.is_empty() {
                return None;
            }
            let joined = rest.iter().map(|s| unquote(s)).collect::<Vec<_>>().join("/");
            params.insert(name.to_string(), joined);
            return Some(params);
        }
        if i >= actual.len() {
            return None;
        }
        let act_seg = actual[i];
        if let Some(name) = pat_seg.strip_prefix(':') {
            params.insert(name.to_string(), unquote(act_seg));
        } else if *pat_seg != act_seg {
            return None;
        }
    }
    if actual.len() != segs.len() {
        return None;
    }
    Some(params)
}

/// Sufijos de formato que seleccionan una representación de un valor `content()`.
const FORMAT_SUFFIXES: [(&str, &str); 3] = [("md", "md"), ("json", "json"), ("html", "html")];

/// Quita un `.md`/`.json`/`.html` final, devolviendo (path_lógico, formato|None).
pub fn split_format_suffix(path: &str) -> (String, Option<String>) {
    for (ext, fmt) in FORMAT_SUFFIXES {
        let dotted = format!(".{}", ext);
        if path.ends_with(&dotted)
            && path.len() > dotted.len()
            && !path[..path.len() - dotted.len()].ends_with('/')
        {
            return (path[..path.len() - dotted.len()].to_string(), Some(fmt.to_string()));
        }
    }
    (path.to_string(), None)
}

/// Mapea un header Accept a un formato de contenido. Default (incl. */*) = HTML.
pub fn negotiate_format(accept: &str) -> String {
    let a = accept.to_lowercase();
    if a.contains("text/markdown") && !a.contains("text/html") {
        return "md".to_string();
    }
    if a.contains("application/json") && !a.contains("text/html") {
        return "json".to_string();
    }
    "html".to_string()
}

// =========================================================
// max_body
// =========================================================

/// Resuelve un setting de max-body a bytes, o None para ilimitado.
/// Acepta número (bytes) o string con unidad ("512kb", "10mb", "1gb") o
/// "unlimited"/"none" para desactivar.
pub fn parse_body_size_str(value: &str) -> Option<i64> {
    let s = value.trim().to_lowercase();
    if matches!(s.as_str(), "unlimited" | "none" | "off" | "0") {
        return None;
    }
    // ^(\d+(?:\.\d+)?)\s*(b|kb|mb|gb)?$
    let mut end = 0;
    let mut seen_dot = false;
    for (idx, c) in s.char_indices() {
        if c.is_ascii_digit() {
            end = idx + 1;
        } else if c == '.' && !seen_dot {
            seen_dot = true;
            end = idx + 1;
        } else {
            break;
        }
    }
    if end == 0 {
        return Some(MAX_BODY);
    }
    let num = &s[..end];
    let rest = s[end..].trim_start();
    let mult: i64 = match rest {
        "" | "b" => 1,
        "kb" => 1024,
        "mb" => 1024 * 1024,
        "gb" => 1024 * 1024 * 1024,
        _ => return Some(MAX_BODY),
    };
    match num.parse::<f64>() {
        Ok(n) => Some((n * mult as f64) as i64),
        Err(_) => Some(MAX_BODY),
    }
}

// =========================================================
// JSON de salida (paridad byte-a-byte con `json.dumps` default de Python:
// separadores ", "/": ", ensure_ascii=True, orden de inserción)
// =========================================================

/// Árbol JSON para la salida. Controlamos el formateo nosotros (no serde) para
/// igualar exactamente a `json.dumps`.
#[derive(Clone, Debug)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Entero de precisión arbitraria: dígitos verbatim (sin comillas).
    BigInt(String),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

fn obj(pairs: Vec<(&str, Json)>) -> Json {
    Json::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Escapa un string como el encoder ascii de Python json: `"` `\` controles y
/// todo lo no-ASCII (≥0x7f) → `\uXXXX` (pares subrogados para >0xFFFF).
fn json_escape_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0a}' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{0d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x7f => out.push(c),
            c => {
                let cp = c as u32;
                if cp <= 0xFFFF {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    let v = cp - 0x10000;
                    let hi = 0xD800 + (v >> 10);
                    let lo = 0xDC00 + (v & 0x3FF);
                    out.push_str(&format!("\\u{:04x}\\u{:04x}", hi, lo));
                }
            }
        }
    }
    out.push('"');
}

fn dumps_into(j: &Json, out: &mut String) {
    match j {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(i) => out.push_str(&i.to_string()),
        Json::BigInt(s) => out.push_str(s),
        Json::Float(f) => {
            if f.is_nan() {
                out.push_str("NaN");
            } else if f.is_infinite() {
                out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" });
            } else {
                out.push_str(&py_float_str(*f));
            }
        }
        Json::Str(s) => json_escape_str(s, out),
        Json::Array(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dumps_into(it, out);
            }
            out.push(']');
        }
        Json::Object(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                json_escape_str(k, out);
                out.push_str(": ");
                dumps_into(v, out);
            }
            out.push('}');
        }
    }
}

/// Serializa como `json.dumps(obj)` (separadores con espacio, ensure_ascii).
pub fn dumps(j: &Json) -> String {
    let mut out = String::new();
    dumps_into(j, &mut out);
    out
}

/// SynValue → árbol JSON (como `syn_to_json` del oráculo).
pub fn syn_to_json(v: &SynValue) -> Json {
    match v {
        SynValue::Nothing => Json::Null,
        SynValue::Bool(b) => Json::Bool(*b),
        SynValue::Number(Number::Int(i)) => Json::Int(*i),
        SynValue::Number(Number::Float(f)) => Json::Float(*f),
        SynValue::Number(Number::Big(b)) => Json::BigInt(b.to_string()),
        SynValue::Text(s) => Json::Str(s.to_string()),
        SynValue::List(l) => Json::Array(l.borrow().iter().map(syn_to_json).collect()),
        SynValue::Map(m) => {
            Json::Object(m.borrow().iter().map(|(k, v)| (k.clone(), syn_to_json(v))).collect())
        }
        SynValue::Task(_) | SynValue::Builtin(_) => Json::Str(v.to_string()),
        SynValue::Server(s) => match &**s {
            // _RAW/_ENVELOPE serializados como data (fuera del contrato) → su dict.
            ServerValue::Raw { body, content_type, status } => obj(vec![
                ("body", Json::Str(body.clone())),
                ("content_type", Json::Str(content_type.clone())),
                ("status", Json::Int(*status)),
            ]),
            ServerValue::Envelope { status, value } => {
                obj(vec![("status", Json::Int(*status)), ("value", syn_to_json(value))])
            }
            // content()/nodo → su árbol JSON estructurado.
            ServerValue::Node(_) => node_to_json(v),
            ServerValue::Content(inner) => node_to_json(inner),
            // paged() fuera del contrato → materializa todo (sin LIMIT).
            ServerValue::Paged(fetch) => match (&**fetch)(None, 0) {
                Ok((rows, _)) => Json::Array(rows.iter().map(syn_to_json).collect()),
                Err(_) => Json::Null,
            },
        },
    }
}

/// serde_json::Value (body entrante parseado) → SynValue (como `python_to_syn`).
pub fn json_to_syn(v: &serde_json::Value) -> SynValue {
    use serde_json::Value as V;
    match v {
        V::Null => syn_nothing(),
        V::Bool(b) => syn_bool(*b),
        V::Number(n) => {
            if let Some(i) = n.as_i64() {
                syn_int(i)
            } else if let Some(f) = n.as_f64() {
                SynValue::Number(Number::Float(f))
            } else {
                syn_int(0)
            }
        }
        V::String(s) => syn_text(s.as_str()),
        V::Array(a) => syn_list(a.iter().map(json_to_syn).collect()),
        V::Object(o) => {
            let mut m = IndexMap::new();
            for (k, val) in o {
                m.insert(k.clone(), json_to_syn(val));
            }
            syn_map(m)
        }
    }
}

// =========================================================
// Contrato de respuesta (paginación de colecciones)
// =========================================================

fn page_window(query: &IndexMap<String, String>) -> (i64, i64) {
    let mut limit = query.get("limit").and_then(|s| s.parse::<i64>().ok()).unwrap_or(DEFAULT_LIMIT);
    if limit <= 0 {
        limit = DEFAULT_LIMIT;
    }
    if limit > MAX_LIMIT {
        limit = MAX_LIMIT;
    }
    let raw_cursor = query.get("cursor").or_else(|| query.get("offset"));
    let mut offset = raw_cursor.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    if offset < 0 {
        offset = 0;
    }
    (limit, offset)
}

fn envelope_from_page(items: Vec<Json>, count: i64, total: i64, limit: i64, offset: i64) -> Json {
    let next = offset + limit;
    let cursor = if next < total { Json::Int(next) } else { Json::Null };
    obj(vec![
        ("items", Json::Array(items)),
        ("count", Json::Int(count)),
        ("total", Json::Int(total)),
        ("cursor", cursor),
    ])
}

fn paginate(items: &[SynValue], query: &IndexMap<String, String>) -> Json {
    let total = items.len() as i64;
    let (limit, offset) = page_window(query);
    let start = offset.min(total).max(0) as usize;
    let end = (offset + limit).min(total).max(0) as usize;
    let page = &items[start..end];
    let page_json: Vec<Json> = page.iter().map(syn_to_json).collect();
    envelope_from_page(page_json, page.len() as i64, total, limit, offset)
}

/// Paginación lazy de `paged()`: sólo se trae la página (LIMIT/OFFSET) y `total`
/// viene de un COUNT(*), sin materializar la colección entera.
fn paginate_lazy(
    fetch: &syntecnia_core::types::PagedFetch,
    query: &IndexMap<String, String>,
) -> Result<Json, String> {
    let (limit, offset) = page_window(query);
    let (rows, total) = fetch(Some(limit), offset)?;
    let count = rows.len() as i64;
    let items: Vec<Json> = rows.iter().map(syn_to_json).collect();
    Ok(envelope_from_page(items, count, total, limit, offset))
}

/// Da forma a un give-value según el contrato (`_shape` del oráculo).
fn shape(value: Option<&SynValue>, query: &IndexMap<String, String>) -> Result<Json, String> {
    match value {
        None | Some(SynValue::Nothing) => Ok(Json::Null),
        Some(SynValue::Server(s)) if matches!(&**s, ServerValue::Paged(_)) => {
            if let ServerValue::Paged(fetch) = &**s {
                paginate_lazy(&**fetch, query)
            } else {
                unreachable!()
            }
        }
        Some(SynValue::List(l)) => Ok(paginate(&l.borrow(), query)),
        Some(v) => Ok(syn_to_json(v)),
    }
}

/// Convierte un give-value en (status, cuerpo) según el contrato. `_RAW` (html/
/// respond/render) se escribe verbatim; `_ENVELOPE` (ok/created/…) lleva status
/// explícito; el resto sigue la forma JSON (paginación de colecciones).
pub fn build_response(
    give: Option<&SynValue>,
    query: &IndexMap<String, String>,
) -> Result<(u16, ResponseBody), String> {
    if let Some(SynValue::Server(s)) = give {
        match &**s {
            ServerValue::Raw { body, content_type, status } => {
                return Ok((
                    *status as u16,
                    ResponseBody::Raw(RawResponse {
                        body: body.clone().into_bytes(),
                        content_type: content_type.clone(),
                        status: *status as u16,
                    }),
                ));
            }
            ServerValue::Envelope { status, value } => {
                return Ok((*status as u16, ResponseBody::Json(shape(Some(value), query)?)));
            }
            _ => {}
        }
    }
    Ok((200, ResponseBody::Json(shape(give, query)?)))
}

// =========================================================
// Tipos del dispatch (handlers inyectados por el motor)
// =========================================================

/// Contexto de una request (lo arma `dispatch`, lo consume el handler del motor).
pub struct Ctx {
    pub method: String,
    pub path: String,
    pub query: IndexMap<String, String>,
    pub params: IndexMap<String, String>,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub body_file: Option<String>,
    pub json: Option<serde_json::Value>,
    pub client_ip: String,
    pub user: Option<SynValue>,
}

/// Resultado de correr el cuerpo de una ruta.
pub enum GiveOutcome {
    /// `give <valor>` (o None si el handler no dio nada → nothing).
    Give(Option<SynValue>),
    /// Violación de `expect` → 400.
    Validation { message: String, field: Option<String> },
    /// Error no capturado → 500.
    Error(String),
}

pub type Handler = Arc<dyn Fn(&Ctx) -> GiveOutcome + Send + Sync>;
pub type AuthHandler = Arc<dyn Fn(&str) -> Option<SynValue> + Send + Sync>;

/// El cliente del SSE se desconectó (escribir al socket falló).
pub struct StreamGone;
/// Emisor de eventos SSE: owned (posee un clone del socket + formatea data:/event:).
/// El motor lo recibe por valor y lo envuelve en su `stream_emit` hook.
pub type Emitter = Box<dyn FnMut(&SynValue, Option<&str>) -> Result<(), StreamGone>>;
/// Cómo terminó un stream (para el evento de error best-effort).
pub enum StreamEnd {
    Done,
    ClientGone,
    Error(String),
}
pub type StreamHandler = Arc<dyn Fn(&Ctx, Emitter) -> StreamEnd + Send + Sync>;

pub struct RouteSpec {
    pub method: String,
    pub path: String,
    pub param_names: Vec<String>,
    pub requires_auth: bool,
    pub streaming: bool,
    pub rate_limit: Option<(i64, f64)>,
    pub rate_zone: Option<String>,
    pub handler: Handler,
    pub stream_handler: Option<StreamHandler>,
}

/// Cuerpo de una respuesta HTTP.
pub enum ResponseBody {
    Json(Json),
    Raw(RawResponse),
}

// =========================================================
// Rate limiter (token bucket, paridad con RateLimiter del oráculo)
// =========================================================

pub struct RateLimiter {
    buckets: Mutex<HashMap<String, (f64, Instant, f64)>>,
    cleanup_interval: f64,
    last_cleanup: Mutex<Option<Instant>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        RateLimiter {
            buckets: Mutex::new(HashMap::new()),
            cleanup_interval: 30.0,
            last_cleanup: Mutex::new(None),
        }
    }

    /// (allowed, remaining, retry_after, reset_seconds).
    pub fn check(&self, key: &str, capacity: i64, window: f64) -> (bool, i64, f64, f64) {
        let now = Instant::now();
        let rate = capacity as f64 / window;
        let mut buckets = self.buckets.lock().unwrap();
        // limpieza perezosa de buckets stale (no vistos por > 2× su ventana)
        {
            let mut lc = self.last_cleanup.lock().unwrap();
            let due = match *lc {
                None => true,
                Some(t) => now.duration_since(t).as_secs_f64() >= self.cleanup_interval,
            };
            if due {
                *lc = Some(now);
                buckets.retain(|_, (_t, last, w)| now.duration_since(*last).as_secs_f64() <= 2.0 * *w);
            }
        }
        let (mut tokens, last, _w) =
            buckets.get(key).copied().unwrap_or((capacity as f64, now, window));
        tokens = (capacity as f64).min(tokens + now.duration_since(last).as_secs_f64() * rate);
        let allowed;
        let retry_after;
        if tokens >= 1.0 {
            tokens -= 1.0;
            allowed = true;
            retry_after = 0.0;
        } else {
            allowed = false;
            retry_after = (1.0 - tokens) / rate;
        }
        buckets.insert(key.to_string(), (tokens, now, window));
        let remaining = tokens as i64;
        let reset = (capacity as f64 - tokens) / rate;
        (allowed, remaining, retry_after, reset)
    }
}

// =========================================================
// ServeRuntime
// =========================================================

/// Tabla de ruteo de un host: rutas + estáticos + auth propios. El host `default`
/// (pattern=None) es el comportamiento de siempre; los vhosts (Lote 1) agregan
/// dominios con su propia tabla, seleccionados por el header `Host`.
pub struct HostRouter {
    /// None = host default (serve-level). Some("a.com") exacto, Some("*.a.com") wildcard.
    pattern: Option<String>,
    routes: Vec<RouteSpec>,
    static_mounts: Vec<(String, PathBuf)>,
    auth_handler: Option<AuthHandler>,
}

impl HostRouter {
    fn new(
        pattern: Option<String>,
        mut routes: Vec<RouteSpec>,
        static_mounts: Vec<(String, String)>,
        auth_handler: Option<AuthHandler>,
    ) -> Self {
        // Orden por especificidad (más específica primero): el primer match gana.
        routes.sort_by(|a, b| specificity(&a.path).cmp(&specificity(&b.path)));
        // Mounts: prefijo normalizado + realpath del directorio, prefijo más largo primero.
        let mut mounts: Vec<(String, PathBuf)> = static_mounts
            .into_iter()
            .map(|(p, d)| {
                let real = Path::new(&d).canonicalize().unwrap_or_else(|_| PathBuf::from(&d));
                (norm_prefix(&p), real)
            })
            .collect();
        mounts.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        HostRouter { pattern, routes, static_mounts: mounts, auth_handler }
    }

    /// ¿Este host (con patrón exacto o `*.dominio`) cubre `host`?
    fn matches_host(&self, host: &str) -> bool {
        match &self.pattern {
            None => true,
            Some(p) => {
                if let Some(suffix) = p.strip_prefix("*.") {
                    let h = host.to_ascii_lowercase();
                    let s = suffix.to_ascii_lowercase();
                    h == s || h.ends_with(&format!(".{}", s))
                } else {
                    host.eq_ignore_ascii_case(p)
                }
            }
        }
    }

    fn match_route(&self, method: &str, path: &str) -> Option<(usize, IndexMap<String, String>)> {
        for (i, route) in self.routes.iter().enumerate() {
            if route.method != method {
                continue;
            }
            if let Some(params) = path_match(&route.path, path) {
                return Some((i, params));
            }
        }
        None
    }

    fn methods_for_path(&self, path: &str) -> Vec<String> {
        let mut methods: Vec<String> = Vec::new();
        for route in &self.routes {
            if path_match(&route.path, path).is_some() && !methods.contains(&route.method) {
                methods.push(route.method.clone());
            }
        }
        methods.sort();
        methods
    }

    /// Sirve un estático de producción: ETag + 304 (If-None-Match), Range/206, gzip
    /// (Accept-Encoding + tipo comprimible). Devuelve status + body + headers extra.
    fn serve_static_full(&self, url_path: &str, headers: &[(String, String)]) -> Option<StaticResp> {
        for (prefix, base) in &self.static_mounts {
            let rel = if prefix == "/" {
                url_path.to_string()
            } else if url_path == prefix.trim_end_matches('/') {
                String::new()
            } else if url_path.starts_with(prefix.as_str()) {
                url_path[prefix.len()..].to_string()
            } else {
                continue;
            };
            let target = match resolve_in(base, &rel) {
                Some(t) => t,
                None => continue,
            };
            let data = match std::fs::read(&target) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let ct = static_content_type(&target);
            let etag = etag_for(&target, data.len());

            // If-None-Match → 304 (sin body).
            let inm = header_value(headers, "if-none-match");
            if !inm.is_empty() && inm.trim() == etag {
                return Some(StaticResp {
                    status: 304,
                    body: Vec::new(),
                    content_type: ct,
                    extra: vec![("ETag".into(), etag), ("Accept-Ranges".into(), "bytes".into())],
                });
            }
            // Range → 206 (sin gzip).
            let range = header_value(headers, "range");
            if !range.is_empty() {
                return Some(match parse_range(&range, data.len()) {
                    Some((start, end)) => StaticResp {
                        status: 206,
                        body: data[start..=end].to_vec(),
                        content_type: ct,
                        extra: vec![
                            ("ETag".into(), etag),
                            ("Accept-Ranges".into(), "bytes".into()),
                            ("Content-Range".into(), format!("bytes {}-{}/{}", start, end, data.len())),
                        ],
                    },
                    None => StaticResp {
                        status: 416,
                        body: Vec::new(),
                        content_type: ct,
                        extra: vec![("Content-Range".into(), format!("bytes */{}", data.len()))],
                    },
                });
            }
            // gzip si el cliente lo acepta y el tipo es comprimible.
            let ae = header_value(headers, "accept-encoding").to_lowercase();
            if ae.contains("gzip") && is_compressible(&ct) {
                if let Some(gz) = gzip_bytes(&data) {
                    return Some(StaticResp {
                        status: 200,
                        body: gz,
                        content_type: ct,
                        extra: vec![
                            ("ETag".into(), etag),
                            ("Accept-Ranges".into(), "bytes".into()),
                            ("Content-Encoding".into(), "gzip".into()),
                            ("Vary".into(), "Accept-Encoding".into()),
                        ],
                    });
                }
            }
            return Some(StaticResp {
                status: 200,
                body: data,
                content_type: ct,
                extra: vec![("ETag".into(), etag), ("Accept-Ranges".into(), "bytes".into())],
            });
        }
        None
    }
}

pub struct ServeRuntime {
    pub port: u16,
    pub host: String,
    pub secure: bool,
    /// A2: TLS activo → se emite HSTS y los `redirect https` apuntan a https://.
    pub tls_enabled: bool,
    /// Host por defecto (rutas/estáticos/auth a nivel de `serve`).
    default_host: HostRouter,
    /// Hosts virtuales (Lote 1): seleccionados por el header `Host`.
    vhosts: Vec<HostRouter>,
    pub max_body: Option<i64>,
    pub max_streams: i64,
    cors_origin: Option<String>,
    intent: Option<String>,
    describe_about: Option<String>,
    describe_api: Vec<String>,
    private: bool,
    rate_limiter: RateLimiter,
    active_streams: Mutex<i64>,
}

/// El resultado de `dispatch`: una respuesta lista, o un hand-off a streaming SSE.
pub enum Dispatched {
    Response { status: u16, body: ResponseBody, headers: Vec<(String, String)> },
    Stream { stream_handler: Option<StreamHandler>, ctx: Box<Ctx> },
}

#[allow(clippy::too_many_arguments)]
impl ServeRuntime {
    pub fn new(
        port: u16,
        host: String,
        routes: Vec<RouteSpec>,
        auth_handler: Option<AuthHandler>,
        max_body: Option<i64>,
        max_streams: i64,
        static_mounts: Vec<(String, String)>,
        cors_origin: Option<String>,
        intent: Option<String>,
        describe_about: Option<String>,
        describe_api: Vec<String>,
        private: bool,
        secure: bool,
    ) -> Self {
        ServeRuntime {
            port,
            host,
            secure,
            tls_enabled: false,
            default_host: HostRouter::new(None, routes, static_mounts, auth_handler),
            vhosts: Vec::new(),
            max_body,
            max_streams,
            cors_origin,
            intent,
            describe_about,
            describe_api,
            private,
            rate_limiter: RateLimiter::new(),
            active_streams: Mutex::new(0),
        }
    }

    /// Registra un vhost (Lote 1): dominio exacto o `*.dominio` con su propia tabla
    /// (rutas/estáticos/auth). Se selecciona por el header `Host`.
    pub fn add_vhost(
        &mut self,
        pattern: String,
        routes: Vec<RouteSpec>,
        static_mounts: Vec<(String, String)>,
        auth_handler: Option<AuthHandler>,
    ) {
        self.vhosts.push(HostRouter::new(Some(pattern), routes, static_mounts, auth_handler));
    }

    /// Selecciona el host por el header `Host`: exacto → wildcard → default.
    fn select_host(&self, host_header: &str) -> &HostRouter {
        if self.vhosts.is_empty() {
            return &self.default_host;
        }
        let h = host_header.split(':').next().unwrap_or("").trim();
        for vh in &self.vhosts {
            if matches!(&vh.pattern, Some(p) if !p.starts_with("*.") && h.eq_ignore_ascii_case(p)) {
                return vh;
            }
        }
        for vh in &self.vhosts {
            if matches!(&vh.pattern, Some(p) if p.starts_with("*.")) && vh.matches_host(h) {
                return vh;
            }
        }
        &self.default_host
    }

    pub fn route_count(&self) -> usize {
        self.default_host.routes.len()
    }
    pub fn cors_origin(&self) -> Option<&str> {
        self.cors_origin.as_deref()
    }

    fn try_acquire_stream(&self) -> bool {
        let mut n = self.active_streams.lock().unwrap();
        if *n >= self.max_streams {
            return false;
        }
        *n += 1;
        true
    }
    fn release_stream(&self) {
        let mut n = self.active_streams.lock().unwrap();
        if *n > 0 {
            *n -= 1;
        }
    }

    /// Métodos permitidos en `path` para el host default (lo usa OPTIONS).
    pub fn methods_for_path(&self, path: &str) -> Vec<String> {
        self.default_host.methods_for_path(path)
    }

    // -- discoverability --

    fn llms_txt(&self) -> String {
        let title = self
            .describe_about
            .clone()
            .or_else(|| self.intent.clone())
            .unwrap_or_else(|| "Syntecnia service".to_string());
        let mut lines = vec![format!("# {}", title)];
        if let Some(intent) = &self.intent {
            if *intent != title {
                lines.push(String::new());
                lines.push(format!("> {}", intent));
            }
        }
        let mut endpoints: Vec<(String, String)> =
            self.default_host.routes.iter().map(|r| (r.method.clone(), r.path.clone())).collect();
        endpoints.sort_by(|a, b| (a.1.clone(), a.0.clone()).cmp(&(b.1.clone(), b.0.clone())));
        endpoints.dedup();
        if !endpoints.is_empty() {
            lines.push(String::new());
            lines.push("## Endpoints".to_string());
            for (m, p) in &endpoints {
                lines.push(format!("- {} {}", m, p));
            }
        }
        if !self.describe_api.is_empty() {
            lines.push(String::new());
            lines.push("## API".to_string());
            for item in &self.describe_api {
                lines.push(format!("- {}", item));
            }
        }
        lines.join("\n") + "\n"
    }

    fn robots_txt(&self) -> String {
        if self.private {
            "User-agent: *\nDisallow: /\n".to_string()
        } else {
            "User-agent: *\nAllow: /\n".to_string()
        }
    }

    fn discovery_response(&self, path: &str) -> Option<RawResponse> {
        if path == "/llms.txt" && !self.private {
            return Some(RawResponse::text(self.llms_txt(), "text/plain; charset=utf-8", 200));
        }
        if path == "/robots.txt" {
            return Some(RawResponse::text(self.robots_txt(), "text/plain; charset=utf-8", 200));
        }
        None
    }

    fn server_error(&self, detail: &str) -> ResponseBody {
        eprintln!("[serve:{}] 500 {}", self.port, detail);
        let body = if self.secure {
            obj(vec![("error", Json::Str("internal server error".into())), ("status", Json::Int(500))])
        } else {
            obj(vec![("error", Json::Str(detail.to_string())), ("status", Json::Int(500))])
        };
        ResponseBody::Json(body)
    }

    // -- dispatch --

    #[allow(clippy::too_many_arguments)]
    pub fn dispatch(
        &self,
        method: &str,
        path: &str,
        query: IndexMap<String, String>,
        headers: Vec<(String, String)>,
        body_str: &str,
        body_file: Option<&str>,
        client_ip: &str,
    ) -> Dispatched {
        let resp = |status, body| Dispatched::Response { status, body, headers: vec![] };

        // vhost (Lote 1): elegir la tabla del host según el header `Host`. Sin vhosts
        // declarados, `host` es siempre el default → comportamiento idéntico al previo.
        let host = self.select_host(&header_value(&headers, "host"));

        let (mut route_idx, mut params) = match host.match_route(method, path) {
            Some((i, p)) => (Some(i), p),
            None => (None, IndexMap::new()),
        };

        // Negociación por sufijo de URL (.md/.json/.html): sólo si un :param se
        // tragó el sufijo. Un estático real en el path exacto gana primero.
        let mut explicit_fmt: Option<String> = None;
        let (logical_path, sfx) = split_format_suffix(path);
        if let (Some(s), Some(idx)) = (sfx, route_idx) {
            if param_last_segment(&host.routes[idx].path) {
                if method == "GET" && !host.static_mounts.is_empty() {
                    if let Some(sr) = host.serve_static_full(path, &headers) {
                        return Dispatched::Response {
                            status: sr.status,
                            body: ResponseBody::Raw(RawResponse {
                                body: sr.body,
                                content_type: sr.content_type,
                                status: sr.status,
                            }),
                            headers: sr.extra,
                        };
                    }
                }
                if let Some((lidx, lparams)) = host.match_route(method, &logical_path) {
                    route_idx = Some(lidx);
                    params = lparams;
                    explicit_fmt = Some(s);
                }
            }
        }

        let idx = match route_idx {
            Some(i) => i,
            None => {
                let allowed = host.methods_for_path(path);
                if !allowed.is_empty() {
                    return Dispatched::Response {
                        status: 405,
                        body: ResponseBody::Json(obj(vec![
                            ("error", Json::Str("method not allowed".into())),
                            ("status", Json::Int(405)),
                        ])),
                        headers: vec![("Allow".to_string(), allowed.join(", "))],
                    };
                }
                if method == "GET" {
                    if !host.static_mounts.is_empty() {
                        if let Some(sr) = host.serve_static_full(path, &headers) {
                            return Dispatched::Response {
                                status: sr.status,
                                body: ResponseBody::Raw(RawResponse {
                                    body: sr.body,
                                    content_type: sr.content_type,
                                    status: sr.status,
                                }),
                                headers: sr.extra,
                            };
                        }
                    }
                    if let Some(disc) = self.discovery_response(path) {
                        return resp(disc.status, ResponseBody::Raw(disc));
                    }
                }
                return resp(
                    404,
                    ResponseBody::Json(obj(vec![
                        ("error", Json::Str(format!("no route for {} {}", method, path))),
                        ("status", Json::Int(404)),
                    ])),
                );
            }
        };

        // Rate limit (tras matchear la ruta, antes de auth/handler).
        let mut rate_headers: Vec<(String, String)> = Vec::new();
        if let Some((capacity, window)) = host.routes[idx].rate_limit {
            let zone = host.routes[idx].rate_zone.clone().unwrap_or_else(|| "None".to_string());
            let key = format!("{}|{}", zone, client_ip);
            let (ok, remaining, retry_after, reset) = self.rate_limiter.check(&key, capacity, window);
            rate_headers = vec![
                ("RateLimit-Limit".to_string(), capacity.to_string()),
                ("RateLimit-Remaining".to_string(), remaining.to_string()),
                ("RateLimit-Reset".to_string(), (reset as i64 + 1).to_string()),
            ];
            if !ok {
                let headers_429 = vec![
                    ("RateLimit-Limit".to_string(), capacity.to_string()),
                    ("RateLimit-Remaining".to_string(), "0".to_string()),
                    ("RateLimit-Reset".to_string(), (reset as i64 + 1).to_string()),
                    ("Retry-After".to_string(), (retry_after as i64 + 1).to_string()),
                ];
                return Dispatched::Response {
                    status: 429,
                    body: ResponseBody::Json(obj(vec![
                        ("error", Json::Str("rate limit exceeded".into())),
                        ("status", Json::Int(429)),
                    ])),
                    headers: headers_429,
                };
            }
        }

        // Parse del body JSON (sólo error si el cliente declaró JSON).
        let mut json_obj: Option<serde_json::Value> = None;
        if !body_str.is_empty() {
            let ctype = header_value(&headers, "content-type").to_lowercase();
            match serde_json::from_str::<serde_json::Value>(body_str) {
                Ok(v) => json_obj = Some(v),
                Err(_) => {
                    if ctype.contains("json") {
                        return Dispatched::Response {
                            status: 400,
                            body: ResponseBody::Json(obj(vec![
                                ("error", Json::Str("malformed JSON body".into())),
                                ("status", Json::Int(400)),
                            ])),
                            headers: vec![],
                        };
                    }
                }
            }
        }

        let mut ctx = Ctx {
            method: method.to_string(),
            path: path.to_string(),
            query,
            params,
            headers: headers.clone(),
            body: body_str.to_string(),
            body_file: body_file.map(|s| s.to_string()),
            json: json_obj,
            client_ip: client_ip.to_string(),
            user: None,
        };

        // Auth.
        if host.routes[idx].requires_auth {
            let token = bearer_token(&headers);
            let user = host.auth_handler.as_ref().and_then(|ah| ah(&token));
            match &user {
                None | Some(SynValue::Nothing) => {
                    return Dispatched::Response {
                        status: 401,
                        body: ResponseBody::Json(obj(vec![
                            ("error", Json::Str("unauthorized".into())),
                            ("status", Json::Int(401)),
                        ])),
                        headers: rate_headers,
                    };
                }
                Some(_) => ctx.user = user,
            }
        }

        // Streaming SSE: adquirir slot y delegar al camino de stream.
        if host.routes[idx].streaming {
            if !self.try_acquire_stream() {
                return Dispatched::Response {
                    status: 503,
                    body: ResponseBody::Json(obj(vec![
                        ("error", Json::Str("too many concurrent streams".into())),
                        ("status", Json::Int(503)),
                    ])),
                    headers: vec![("Retry-After".to_string(), "5".to_string())],
                };
            }
            return Dispatched::Stream {
                stream_handler: host.routes[idx].stream_handler.clone(),
                ctx: Box::new(ctx),
            };
        }

        // Correr el handler.
        let (status, body) = match (host.routes[idx].handler)(&ctx) {
            GiveOutcome::Give(v) => {
                let is_content = matches!(
                    v.as_ref(),
                    Some(SynValue::Server(s)) if matches!(&**s, ServerValue::Content(_))
                );
                if is_content {
                    // content() se negocia: sufijo explícito (.md/.json/.html) gana,
                    // si no el header Accept (default HTML).
                    let fmt = explicit_fmt
                        .clone()
                        .unwrap_or_else(|| negotiate_format(&header_value(&ctx.headers, "accept")));
                    let raw = render_content(v.as_ref().unwrap(), &fmt);
                    (raw.status, ResponseBody::Raw(raw))
                } else {
                    match build_response(v.as_ref(), &ctx.query) {
                        Ok(sb) => sb,
                        Err(e) => (500, self.server_error(&e)),
                    }
                }
            }
            GiveOutcome::Validation { message, field } => (
                400,
                ResponseBody::Json(obj(vec![
                    ("error", Json::Str(message)),
                    ("status", Json::Int(400)),
                    ("field", field.map(Json::Str).unwrap_or(Json::Null)),
                ])),
            ),
            GiveOutcome::Error(msg) => (500, self.server_error(&msg)),
        };
        Dispatched::Response { status, body, headers: rate_headers }
    }
}

// -- helpers de matching/estáticos (libres) --

fn norm_prefix(prefix: &str) -> String {
    if prefix.is_empty() || prefix == "/" {
        return "/".to_string();
    }
    format!("/{}/", prefix.trim_matches('/'))
}

fn within(base: &Path, target: &Path) -> bool {
    target == base || target.starts_with(base)
}

/// Resuelve `rel` a un archivo real dentro de `base` (anti-traversal), o None.
fn resolve_in(base: &Path, rel: &str) -> Option<PathBuf> {
    let rel = unquote(rel);
    let rel = rel.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    // Path absoluto o con drive-letter (C:) no puede estar dentro de uno relativo.
    if Path::new(rel).is_absolute() || (rel.len() > 1 && rel.as_bytes()[1] == b':') {
        return None;
    }
    let mut target = base.join(rel).canonicalize().ok()?;
    if !within(base, &target) {
        return None;
    }
    if target.is_dir() {
        target = target.join("index.html").canonicalize().ok()?;
        if !within(base, &target) {
            return None;
        }
    }
    if !target.is_file() {
        return None;
    }
    Some(target)
}

fn static_content_type(path: &Path) -> String {
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    // Web types pinneadas (el contrato, deterministas entre hosts).
    if let Some(t) = web_content_type(&ext) {
        return t.to_string();
    }
    // Fallback: mimetypes.guess_type (tabla incorporada + registro de Windows).
    match crate::mimetypes::guess(&ext) {
        None => "application/octet-stream".to_string(),
        Some(ct) => {
            if ct.starts_with("text/") && !ct.contains("charset") {
                format!("{}; charset=utf-8", ct)
            } else {
                ct
            }
        }
    }
}

fn header_value(headers: &[(String, String)], name: &str) -> String {
    for (k, v) in headers {
        if k.eq_ignore_ascii_case(name) {
            return v.clone();
        }
    }
    String::new()
}

fn bearer_token(headers: &[(String, String)]) -> String {
    let auth = header_value(headers, "authorization");
    if auth.is_empty() {
        return String::new();
    }
    let mut parts = auth.splitn(2, char::is_whitespace);
    let scheme = parts.next().unwrap_or("");
    if let Some(rest) = parts.next() {
        if scheme.eq_ignore_ascii_case("bearer") {
            return rest.trim().to_string();
        }
    }
    auth.trim().to_string()
}

// =========================================================
// Árbol de contenido semántico (vocabulario content()) + renderers
// =========================================================

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

fn num_i64(v: &SynValue) -> i64 {
    match v {
        SynValue::Number(Number::Int(i)) => *i,
        SynValue::Number(Number::Float(f)) => *f as i64,
        SynValue::Number(Number::Big(b)) => b.to_string().parse().unwrap_or(0),
        _ => 0,
    }
}

fn is_node(v: &SynValue) -> bool {
    matches!(v, SynValue::Server(s) if matches!(&**s, ServerValue::Node(_)))
}

fn node_field(v: &SynValue, key: &str) -> Option<SynValue> {
    if let SynValue::Server(s) = v {
        s.get_field(key)
    } else {
        None
    }
}

fn node_str(v: &SynValue, key: &str) -> String {
    match node_field(v, key) {
        None | Some(SynValue::Nothing) => String::new(),
        Some(x) => x.to_string(),
    }
}

fn node_int(v: &SynValue, key: &str, default: i64) -> i64 {
    match node_field(v, key) {
        Some(n @ SynValue::Number(_)) => num_i64(&n),
        _ => default,
    }
}

fn list_field(v: &SynValue, key: &str) -> Vec<SynValue> {
    match node_field(v, key) {
        Some(SynValue::List(l)) => l.borrow().clone(),
        _ => Vec::new(),
    }
}

fn meta_get(meta: &SynValue, key: &str) -> Option<String> {
    if let SynValue::Map(m) = meta {
        m.borrow().get(key).map(|v| v.to_string())
    } else {
        None
    }
}

// -- JSON (el árbol como data) --

fn meta_to_json(meta: Option<&SynValue>) -> Json {
    match meta {
        Some(SynValue::Map(m)) => {
            Json::Object(m.borrow().iter().map(|(k, v)| (k.clone(), syn_to_json(v))).collect())
        }
        _ => Json::Object(Vec::new()),
    }
}

fn item_to_json(item: &SynValue) -> Json {
    if is_node(item) {
        node_to_json(item)
    } else {
        syn_to_json(item)
    }
}

fn node_to_json(node: &SynValue) -> Json {
    let kind = node_str(node, "kind");
    match kind.as_str() {
        "page" => obj(vec![
            ("type", Json::Str("page".into())),
            ("meta", meta_to_json(node_field(node, "meta").as_ref())),
            (
                "nodes",
                Json::Array(
                    list_field(node, "nodes").iter().filter(|n| is_node(n)).map(node_to_json).collect(),
                ),
            ),
        ]),
        "list" | "ordered_list" => obj(vec![
            ("type", Json::Str(kind.clone())),
            ("items", Json::Array(list_field(node, "items").iter().map(item_to_json).collect())),
        ]),
        "section" => obj(vec![
            ("type", Json::Str("section".into())),
            (
                "nodes",
                Json::Array(
                    list_field(node, "nodes").iter().filter(|n| is_node(n)).map(node_to_json).collect(),
                ),
            ),
        ]),
        _ => {
            let mut pairs: Vec<(String, Json)> = vec![("type".to_string(), Json::Str(kind))];
            for key in ["level", "text", "href", "src", "alt", "lang", "html"] {
                if let Some(val) = node_field(node, key) {
                    if !matches!(val, SynValue::Nothing) {
                        pairs.push((key.to_string(), syn_to_json(&val)));
                    }
                }
            }
            Json::Object(pairs)
        }
    }
}

// -- HTML (semántico + <head> desde la metadata) --

fn render_li(item: &SynValue) -> String {
    if is_node(item) {
        format!("<li>{}</li>", render_node_html(item))
    } else {
        format!("<li>{}</li>", esc(&item.to_string()))
    }
}

fn render_node_html(node: &SynValue) -> String {
    let kind = node_str(node, "kind");
    match kind.as_str() {
        "heading" => {
            let lvl = node_int(node, "level", 1).clamp(1, 6);
            format!("<h{0}>{1}</h{0}>\n", lvl, esc(&node_str(node, "text")))
        }
        "prose" => format!("<p>{}</p>\n", esc(&node_str(node, "text"))),
        "list" | "ordered_list" => {
            let tag = if kind == "ordered_list" { "ol" } else { "ul" };
            let inner: String = list_field(node, "items").iter().map(render_li).collect();
            format!("<{0}>{1}</{0}>\n", tag, inner)
        }
        "link" => format!(
            "<a href=\"{}\">{}</a>\n",
            esc(&node_str(node, "href")),
            esc(&node_str(node, "text"))
        ),
        "image" => format!(
            "<img src=\"{}\" alt=\"{}\">\n",
            esc(&node_str(node, "src")),
            esc(&node_str(node, "alt"))
        ),
        "section" => {
            let inner: String =
                list_field(node, "nodes").iter().filter(|n| is_node(n)).map(render_node_html).collect();
            format!("<section>\n{}</section>\n", inner)
        }
        "code" => {
            let lang = node_str(node, "lang");
            let cls = if lang.is_empty() {
                String::new()
            } else {
                format!(" class=\"language-{}\"", esc(&lang))
            };
            format!("<pre><code{}>{}</code></pre>\n", cls, esc(&node_str(node, "text")))
        }
        "raw" => node_str(node, "html"), // escape hatch: NO escapado
        "page" => list_field(node, "nodes").iter().filter(|n| is_node(n)).map(render_node_html).collect(),
        _ => String::new(),
    }
}

fn render_html(tree: &SynValue) -> String {
    let is_page = node_str(tree, "kind") == "page";
    let meta = if is_page { node_field(tree, "meta") } else { None };
    let title = meta.as_ref().and_then(|m| meta_get(m, "title"));
    let description = meta.as_ref().and_then(|m| meta_get(m, "description"));
    let mut head = vec![
        "<meta charset=\"utf-8\">".to_string(),
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">".to_string(),
    ];
    if let Some(t) = &title {
        head.push(format!("<title>{}</title>", esc(t)));
    }
    if let Some(d) = &description {
        head.push(format!("<meta name=\"description\" content=\"{}\">", esc(d)));
    }
    if title.is_some() || description.is_some() {
        let mut ld: Vec<(&str, Json)> = vec![
            ("@context", Json::Str("https://schema.org".into())),
            ("@type", Json::Str("WebPage".into())),
        ];
        if let Some(t) = &title {
            ld.push(("name", Json::Str(t.clone())));
        }
        if let Some(d) = &description {
            ld.push(("description", Json::Str(d.clone())));
        }
        // Escapar < > & como \uXXXX para no romper el <script> (XSS-safe).
        let ld_json = dumps(&obj(ld))
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026");
        head.push(format!("<script type=\"application/ld+json\">{}</script>", ld_json));
    }
    let body = if is_page {
        list_field(tree, "nodes").iter().filter(|n| is_node(n)).map(render_node_html).collect::<String>()
    } else {
        render_node_html(tree)
    };
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n{}\n</head>\n<body>\n{}</body>\n</html>\n",
        head.join("\n"),
        body
    )
}

// -- Markdown (para agentes) --

fn md_inline(item: &SynValue) -> String {
    if is_node(item) {
        render_node_md(item).trim().to_string()
    } else {
        item.to_string()
    }
}

fn render_node_md(node: &SynValue) -> String {
    let kind = node_str(node, "kind");
    match kind.as_str() {
        "heading" => {
            let lvl = node_int(node, "level", 1).clamp(1, 6) as usize;
            format!("{} {}", "#".repeat(lvl), node_str(node, "text"))
        }
        "prose" => node_str(node, "text"),
        "list" => list_field(node, "items")
            .iter()
            .map(|i| format!("- {}", md_inline(i)))
            .collect::<Vec<_>>()
            .join("\n"),
        "ordered_list" => list_field(node, "items")
            .iter()
            .enumerate()
            .map(|(n, i)| format!("{}. {}", n + 1, md_inline(i)))
            .collect::<Vec<_>>()
            .join("\n"),
        "link" => format!("[{}]({})", node_str(node, "text"), node_str(node, "href")),
        "image" => format!("![{}]({})", node_str(node, "alt"), node_str(node, "src")),
        "section" => list_field(node, "nodes")
            .iter()
            .filter(|n| is_node(n))
            .map(render_node_md)
            .collect::<Vec<_>>()
            .join("\n\n"),
        "code" => format!("```{}\n{}\n```", node_str(node, "lang"), node_str(node, "text")),
        "raw" => node_str(node, "html"),
        "page" => list_field(node, "nodes")
            .iter()
            .filter(|n| is_node(n))
            .map(render_node_md)
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

fn render_markdown(tree: &SynValue) -> String {
    let body = if node_str(tree, "kind") == "page" {
        list_field(tree, "nodes")
            .iter()
            .filter(|n| is_node(n))
            .map(render_node_md)
            .collect::<Vec<_>>()
            .join("\n\n")
    } else {
        render_node_md(tree)
    };
    format!("{}\n", body.trim_end())
}

/// Renderiza un valor `content()` en el formato elegido como RawResponse.
pub fn render_content(content_value: &SynValue, fmt: &str) -> RawResponse {
    let tree = match content_value {
        SynValue::Server(s) => match &**s {
            ServerValue::Content(inner) => inner.as_ref(),
            _ => content_value,
        },
        _ => content_value,
    };
    match fmt {
        "json" => RawResponse::text(dumps(&node_to_json(tree)), "application/json; charset=utf-8", 200),
        "md" => RawResponse::text(render_markdown(tree), "text/markdown; charset=utf-8", 200),
        _ => RawResponse::text(render_html(tree), "text/html; charset=utf-8", 200),
    }
}

// =========================================================
// Builtins de respuesta + vocabulario de contenido
// =========================================================

fn text_arg(v: Option<&SynValue>) -> String {
    match v {
        None => String::new(),
        Some(SynValue::Text(s)) => s.to_string(),
        Some(o) => o.to_string(),
    }
}

fn make_raw_val(body: String, ct: &str, status: i64) -> SynValue {
    SynValue::Server(Rc::new(ServerValue::Raw { body, content_type: ct.to_string(), status }))
}

fn make_envelope(status: i64, value: SynValue) -> SynValue {
    SynValue::Server(Rc::new(ServerValue::Envelope { status, value }))
}

fn n_nodes(v: Option<&SynValue>) -> SynValue {
    match v {
        Some(SynValue::List(l)) => syn_list(l.borrow().clone()),
        _ => syn_list(Vec::new()),
    }
}

fn n_meta(v: Option<&SynValue>) -> SynValue {
    match v {
        Some(SynValue::Map(m)) => {
            let mut out = IndexMap::new();
            for (k, val) in m.borrow().iter() {
                out.insert(k.clone(), syn_text(text_arg(Some(val))));
            }
            syn_map(out)
        }
        _ => syn_map(IndexMap::new()),
    }
}

fn make_node(kind: &str, fields: Vec<(&str, SynValue)>) -> SynValue {
    let mut m: IndexMap<String, SynValue> = IndexMap::new();
    m.insert("kind".to_string(), syn_text(kind));
    for (k, v) in fields {
        m.insert(k.to_string(), v);
    }
    SynValue::Server(Rc::new(ServerValue::Node(Rc::new(RefCell::new(m)))))
}

/// Registra los helpers de respuesta (ok/created/not_found/fail/html/respond) y el
/// vocabulario de contenido (page/heading/prose/list/…/content). El oráculo los
/// registra en el intérprete principal SIEMPRE → acá van en cada intérprete.
pub fn register_serve_builtins(interp: &Interpreter) {
    interp.register_builtin(
        "ok",
        1,
        Rc::new(|_i, a, _l| Ok(make_envelope(200, a.first().cloned().unwrap_or_else(syn_nothing)))),
    );
    interp.register_builtin(
        "created",
        1,
        Rc::new(|_i, a, _l| Ok(make_envelope(201, a.first().cloned().unwrap_or_else(syn_nothing)))),
    );
    interp.register_builtin(
        "not_found",
        1,
        Rc::new(|_i, a, _l| {
            let value = a.first().cloned().unwrap_or_else(|| syn_text("not found"));
            let value = if matches!(value, SynValue::Map(_)) {
                value
            } else {
                let mut m = IndexMap::new();
                m.insert("error".to_string(), syn_text(value.to_string()));
                m.insert("status".to_string(), syn_int(404));
                syn_map(m)
            };
            Ok(make_envelope(404, value))
        }),
    );
    interp.register_builtin(
        "fail",
        -1,
        Rc::new(|_i, a, _l| {
            let mut code = 400i64;
            let mut msg = "error".to_string();
            if a.len() >= 2 {
                if matches!(a[0], SynValue::Number(_)) {
                    code = num_i64(&a[0]);
                    msg = a[1].to_string();
                } else {
                    msg = a[0].to_string();
                    if matches!(a[1], SynValue::Number(_)) {
                        code = num_i64(&a[1]);
                    }
                }
            } else if a.len() == 1 {
                if matches!(a[0], SynValue::Number(_)) {
                    code = num_i64(&a[0]);
                } else {
                    msg = a[0].to_string();
                }
            }
            let mut body = IndexMap::new();
            body.insert("error".to_string(), syn_text(msg));
            body.insert("status".to_string(), syn_int(code));
            Ok(make_envelope(code, syn_map(body)))
        }),
    );
    interp.register_builtin(
        "html",
        1,
        Rc::new(|_i, a, _l| Ok(make_raw_val(text_arg(a.first()), "text/html; charset=utf-8", 200))),
    );
    interp.register_builtin(
        "respond",
        -1,
        Rc::new(|_i, a, _l| {
            let content = text_arg(a.first());
            let ct = if a.len() > 1 {
                text_arg(a.get(1))
            } else {
                "text/plain; charset=utf-8".to_string()
            };
            let status = match a.get(2) {
                Some(n @ SynValue::Number(_)) => num_i64(n),
                _ => 200,
            };
            Ok(make_raw_val(content, &ct, status))
        }),
    );
    // Vocabulario de contenido semántico.
    interp.register_builtin(
        "page",
        -1,
        Rc::new(|_i, a, _l| {
            Ok(make_node(
                "page",
                vec![("nodes", n_nodes(a.first())), ("meta", n_meta(a.get(1)))],
            ))
        }),
    );
    interp.register_builtin(
        "heading",
        2,
        Rc::new(|_i, a, _l| {
            let level = match a.first() {
                Some(n @ SynValue::Number(_)) => num_i64(n),
                _ => 1,
            };
            Ok(make_node(
                "heading",
                vec![("level", syn_int(level)), ("text", syn_text(text_arg(a.get(1))))],
            ))
        }),
    );
    interp.register_builtin(
        "prose",
        1,
        Rc::new(|_i, a, _l| Ok(make_node("prose", vec![("text", syn_text(text_arg(a.first())))]))),
    );
    interp.register_builtin(
        "list",
        1,
        Rc::new(|_i, a, _l| Ok(make_node("list", vec![("items", n_nodes(a.first()))]))),
    );
    interp.register_builtin(
        "ordered_list",
        1,
        Rc::new(|_i, a, _l| Ok(make_node("ordered_list", vec![("items", n_nodes(a.first()))]))),
    );
    interp.register_builtin(
        "link",
        2,
        Rc::new(|_i, a, _l| {
            Ok(make_node(
                "link",
                vec![("text", syn_text(text_arg(a.first()))), ("href", syn_text(text_arg(a.get(1))))],
            ))
        }),
    );
    interp.register_builtin(
        "image",
        2,
        Rc::new(|_i, a, _l| {
            Ok(make_node(
                "image",
                vec![("src", syn_text(text_arg(a.first()))), ("alt", syn_text(text_arg(a.get(1))))],
            ))
        }),
    );
    interp.register_builtin(
        "section",
        1,
        Rc::new(|_i, a, _l| Ok(make_node("section", vec![("nodes", n_nodes(a.first()))]))),
    );
    interp.register_builtin(
        "code",
        -1,
        Rc::new(|_i, a, _l| {
            let lang = if a.len() > 1 { syn_text(text_arg(a.get(1))) } else { syn_nothing() };
            Ok(make_node("code", vec![("text", syn_text(text_arg(a.first()))), ("lang", lang)]))
        }),
    );
    interp.register_builtin(
        "raw",
        1,
        Rc::new(|_i, a, _l| Ok(make_node("raw", vec![("html", syn_text(text_arg(a.first())))]))),
    );
    interp.register_builtin(
        "content",
        1,
        Rc::new(|_i, a, _l| {
            let tree = a
                .first()
                .cloned()
                .unwrap_or_else(|| make_node("page", vec![("nodes", syn_list(Vec::new())), ("meta", syn_map(IndexMap::new()))]));
            Ok(SynValue::Server(Rc::new(ServerValue::Content(Box::new(tree)))))
        }),
    );
}

// =========================================================
// Servidor HTTP (thread-per-connection, std::net)
// =========================================================

/// Stack grande por hilo de conexión: el handler corre un intérprete tree-walking
/// (recursión profunda), igual que los hilos de agentes.
const CONN_STACK_SIZE: usize = 256 * 1024 * 1024;

/// Frase de razón de `http.HTTPStatus` (Python 3.12). Códigos fuera de ese set →
/// "" (como `send_response_only` cuando el código no está en `responses`).
fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Request Entity Too Large",
        414 => "Request-URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Requested Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a Teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Entity",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "",
    }
}

enum BodyRead {
    Bytes(Vec<u8>),
    /// Body grande spilled a un temp file (el caller lo borra al terminar).
    File(PathBuf),
    TooLarge,
}

/// Path único para un body spilled (como tempfile.mkstemp(prefix="syn_body_")).
fn spill_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("syn_body_{}_{}", std::process::id(), n))
}

/// Procesa un bloque del body: cuenta bytes, spillea a disco si supera `spill_at`.
/// Devuelve false si excede `max_body` (→ TooLarge).
fn feed_block(
    block: &[u8],
    total: &mut usize,
    buf: &mut Vec<u8>,
    tmp: &mut Option<(File, PathBuf)>,
    max_body: Option<i64>,
    spill_at: usize,
) -> bool {
    if block.is_empty() {
        return true;
    }
    *total += block.len();
    if let Some(mb) = max_body {
        if *total as i64 > mb {
            return false;
        }
    }
    if tmp.is_none() {
        buf.extend_from_slice(block);
        if buf.len() > spill_at {
            let path = spill_path();
            if let Ok(mut f) = File::create(&path) {
                let _ = f.write_all(buf);
                buf.clear();
                *tmp = Some((f, path));
            }
        }
    } else if let Some((f, _)) = tmp.as_mut() {
        let _ = f.write_all(block);
    }
    true
}

/// Loop de accept: un hilo (con stack grande) por conexión. Bloquea para siempre.
pub fn serve_forever(rt: Arc<ServeRuntime>, listener: TcpListener) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = stream.set_nodelay(true);
        let client_ip = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
        let rt = rt.clone();
        let _ = std::thread::Builder::new()
            .stack_size(CONN_STACK_SIZE)
            .spawn(move || {
                let _ = handle_connection(rt, stream, client_ip);
            });
    }
}

/// A2 — Loop de accept TLS: igual que `serve_forever` pero envuelve cada conexión
/// en rustls (backend ring). El handshake ocurre perezosamente al primer read; si
/// falla, esa conexión se cierra limpiamente sin tirar el servidor.
pub fn serve_forever_tls(
    rt: Arc<ServeRuntime>,
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
) {
    for stream in listener.incoming() {
        let tcp = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = tcp.set_nodelay(true);
        let client_ip = tcp.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
        let conn = match rustls::ServerConnection::new(config.clone()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let rt = rt.clone();
        let _ = std::thread::Builder::new()
            .stack_size(CONN_STACK_SIZE)
            .spawn(move || {
                let tls = rustls::StreamOwned::new(conn, tcp);
                let _ = handle_connection(rt, tls, client_ip);
            });
    }
}

/// A2 batch 2 — Mapa compartido token→key-authorization que el listener HTTP sirve
/// para los challenges ACME HTTP-01.
pub type ChallengeStore = std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>;

/// A2 batch 2 — Config TLS compartida y mutable (renovación ACME hace hot-swap).
pub type SharedServerConfig = std::sync::Arc<std::sync::RwLock<Arc<rustls::ServerConfig>>>;

/// A2 batch 2 — Igual que `serve_forever_tls` pero lee la config desde una celda
/// compartida en cada accept, para que la renovación ACME pueda hot-swappear el cert
/// sin reiniciar el server.
pub fn serve_forever_tls_auto(
    rt: Arc<ServeRuntime>,
    listener: TcpListener,
    config: SharedServerConfig,
) {
    for stream in listener.incoming() {
        let tcp = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = tcp.set_nodelay(true);
        let client_ip = tcp.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
        let cfg = config.read().unwrap().clone();
        let conn = match rustls::ServerConnection::new(cfg) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let rt = rt.clone();
        let _ = std::thread::Builder::new()
            .stack_size(CONN_STACK_SIZE)
            .spawn(move || {
                let tls = rustls::StreamOwned::new(conn, tcp);
                let _ = handle_connection(rt, tls, client_ip);
            });
    }
}

/// A2 batch 2 — Listener HTTP (típico :80) para auto-HTTPS: sirve el challenge
/// ACME HTTP-01 (`/.well-known/acme-challenge/<token>` → key-authorization desde
/// `store`) y redirige todo lo demás a https. Reúsa el rol del listener de redirect.
pub fn serve_acme_http(listener: TcpListener, https_port: u16, store: ChallengeStore) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let store = store.clone();
        let _ = std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(move || {
                let _ = acme_http_one(stream, https_port, store);
            });
    }
}

fn acme_http_one(stream: TcpStream, https_port: u16, store: ChallengeStore) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    let (_, target, _, headers) = match read_request_head(&mut reader) {
        Some(x) => x,
        None => return Ok(()),
    };
    let (path, _q) = parse_path_query(&target);
    const PFX: &str = "/.well-known/acme-challenge/";
    if let Some(token) = path.strip_prefix(PFX) {
        let body = store.lock().unwrap().get(token).cloned();
        let stream = reader.get_mut();
        match body {
            Some(key_auth) => {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    key_auth.len(),
                    key_auth
                );
                stream.write_all(resp.as_bytes())?;
            }
            None => {
                stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )?;
            }
        }
        return stream.flush();
    }
    // No es un challenge → 301 a https.
    let host_hdr = header_value(&headers, "host");
    let host = host_hdr.split(':').next().unwrap_or("").to_string();
    let authority = if https_port == 443 || host.is_empty() {
        host
    } else {
        format!("{}:{}", host, https_port)
    };
    let resp = format!(
        "HTTP/1.1 301 Moved Permanently\r\nLocation: https://{}{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        authority, target
    );
    let stream = reader.get_mut();
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

/// A2 — Construye una `ServerConfig` de rustls (ring) desde cert+key PEM. Defaults
/// seguros: TLS 1.2+ (versiones por defecto de rustls), sin auth de cliente.
pub fn build_tls_config(cert_path: &str, key_path: &str) -> Result<Arc<rustls::ServerConfig>, String> {
    let cert_file =
        File::open(cert_path).map_err(|e| format!("could not read TLS cert {}: {}", cert_path, e))?;
    let mut cert_rd = BufReader::new(cert_file);
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_rd)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("invalid TLS cert {}: {}", cert_path, e))?;
    if certs.is_empty() {
        return Err(format!("no certificates found in {}", cert_path));
    }
    let key_file =
        File::open(key_path).map_err(|e| format!("could not read TLS key {}: {}", key_path, e))?;
    let mut key_rd = BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_rd)
        .map_err(|e| format!("invalid TLS key {}: {}", key_path, e))?
        .ok_or_else(|| format!("no private key found in {}", key_path))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS config error: {}", e))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS cert/key mismatch: {}", e))?;
    Ok(Arc::new(config))
}

/// Carga un par cert(cadena)+key PEM en un `CertifiedKey` de rustls (para SNI por-host).
fn load_certified_key(cert_path: &str, key_path: &str) -> Result<rustls::sign::CertifiedKey, String> {
    let cert_file =
        File::open(cert_path).map_err(|e| format!("could not read TLS cert {}: {}", cert_path, e))?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut BufReader::new(cert_file))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("invalid TLS cert {}: {}", cert_path, e))?;
    if certs.is_empty() {
        return Err(format!("no certificates found in {}", cert_path));
    }
    let key_file =
        File::open(key_path).map_err(|e| format!("could not read TLS key {}: {}", key_path, e))?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .map_err(|e| format!("invalid TLS key {}: {}", key_path, e))?
        .ok_or_else(|| format!("no private key found in {}", key_path))?;
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| format!("unsupported TLS key in {}: {}", key_path, e))?;
    Ok(rustls::sign::CertifiedKey::new(certs, signing_key))
}

/// Resolver SNI (vhost): elige el cert por server name del handshake; cae al default.
#[derive(Debug)]
struct SniResolver {
    default: std::sync::Arc<rustls::sign::CertifiedKey>,
    by_name: HashMap<String, std::sync::Arc<rustls::sign::CertifiedKey>>,
    wildcards: Vec<(String, std::sync::Arc<rustls::sign::CertifiedKey>)>,
}

impl rustls::server::ResolvesServerCert for SniResolver {
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello,
    ) -> Option<std::sync::Arc<rustls::sign::CertifiedKey>> {
        if let Some(name) = client_hello.server_name() {
            let name = name.to_ascii_lowercase();
            if let Some(ck) = self.by_name.get(&name) {
                return Some(ck.clone());
            }
            for (suffix, ck) in &self.wildcards {
                if name.ends_with(suffix) {
                    return Some(ck.clone());
                }
            }
        }
        Some(self.default.clone())
    }
}

/// A2 — Config TLS con SNI por-host (vhost): `default_*` es el fallback; cada host
/// (exacto o `*.dominio`) presenta su propio cert. Defaults seguros (TLS 1.2+).
pub fn build_tls_config_sni(
    default_cert: &str,
    default_key: &str,
    hosts: Vec<(String, String, String)>,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let default = std::sync::Arc::new(load_certified_key(default_cert, default_key)?);
    let mut by_name: HashMap<String, std::sync::Arc<rustls::sign::CertifiedKey>> = HashMap::new();
    let mut wildcards: Vec<(String, std::sync::Arc<rustls::sign::CertifiedKey>)> = Vec::new();
    for (pattern, cert, key) in hosts {
        let ck = std::sync::Arc::new(load_certified_key(&cert, &key)?);
        if let Some(suffix) = pattern.strip_prefix("*.") {
            wildcards.push((format!(".{}", suffix.to_ascii_lowercase()), ck));
        } else {
            by_name.insert(pattern.to_ascii_lowercase(), ck);
        }
    }
    let resolver = std::sync::Arc::new(SniResolver { default, by_name, wildcards });
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS config error: {}", e))?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    Ok(Arc::new(config))
}

/// A2 — Loop de redirección: escucha (típico :80) y responde 301 a https://host[:port].
/// Lee sólo la request line + headers; un hilo liviano por request.
pub fn serve_redirect(listener: TcpListener, https_port: u16) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(move || {
                let _ = redirect_one(stream, https_port);
            });
    }
}

fn redirect_one(stream: TcpStream, https_port: u16) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    let (_, target, _, headers) = match read_request_head(&mut reader) {
        Some(x) => x,
        None => return Ok(()),
    };
    let host_hdr = header_value(&headers, "host");
    let host = host_hdr.split(':').next().unwrap_or("").to_string();
    let authority = if https_port == 443 || host.is_empty() {
        host
    } else {
        format!("{}:{}", host, https_port)
    };
    let resp = format!(
        "HTTP/1.1 301 Moved Permanently\r\nLocation: https://{}{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        authority, target
    );
    let stream = reader.get_mut();
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

fn send_head<W: Write>(stream: &mut W, status: u16, headers: &[(String, String)]) -> io::Result<()> {
    let mut h = format!("HTTP/1.1 {} {}\r\n", status, reason(status));
    for (k, v) in headers {
        h.push_str(k);
        h.push_str(": ");
        h.push_str(v);
        h.push_str("\r\n");
    }
    h.push_str("\r\n");
    stream.write_all(h.as_bytes())
}

fn read_request_head<R: BufRead>(
    reader: &mut R,
) -> Option<(String, String, String, Vec<(String, String)>)> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None; // EOF / conexión cerrada
    }
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return None;
    }
    let mut it = line.splitn(3, ' ');
    let method = it.next()?.to_string();
    let target = it.next()?.to_string();
    let version = it.next().unwrap_or("HTTP/1.1").to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Some((method, target, version, headers))
}

fn read_body<R: BufRead>(
    reader: &mut R,
    headers: &[(String, String)],
    max_body: Option<i64>,
) -> BodyRead {
    // Spillea a disco sobre MEM_SPILL (capado a max_body). Cuenta bytes reales,
    // nunca confía en Content-Length.
    let spill_at = match max_body {
        None => MEM_SPILL,
        Some(mb) => (mb.max(0) as usize).min(MEM_SPILL),
    };
    let mut total: usize = 0;
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp: Option<(File, PathBuf)> = None;
    let mut too_large = false;

    let te = header_value(headers, "transfer-encoding").to_lowercase();
    if te.contains("chunked") {
        loop {
            let mut size_line = String::new();
            if reader.read_line(&mut size_line).unwrap_or(0) == 0 {
                break;
            }
            let hex = size_line.trim().split(';').next().unwrap_or("").trim().to_string();
            if hex.is_empty() {
                continue;
            }
            let sz = match usize::from_str_radix(&hex, 16) {
                Ok(s) => s,
                Err(_) => break,
            };
            if sz == 0 {
                let mut crlf = String::new();
                let _ = reader.read_line(&mut crlf);
                break;
            }
            let mut chunk = vec![0u8; sz];
            if reader.read_exact(&mut chunk).is_err() {
                break;
            }
            if !feed_block(&chunk, &mut total, &mut buf, &mut tmp, max_body, spill_at) {
                too_large = true;
                break;
            }
            let mut crlf = String::new();
            let _ = reader.read_line(&mut crlf);
        }
    } else {
        let mut remaining: usize =
            header_value(headers, "content-length").trim().parse().unwrap_or(0);
        let mut block = vec![0u8; 65536];
        while remaining > 0 {
            let want = remaining.min(block.len());
            let n = match reader.read(&mut block[..want]) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if !feed_block(&block[..n], &mut total, &mut buf, &mut tmp, max_body, spill_at) {
                too_large = true;
                break;
            }
            remaining -= n;
        }
    }

    if too_large {
        if let Some((_, path)) = tmp {
            let _ = std::fs::remove_file(path);
        }
        return BodyRead::TooLarge;
    }
    match tmp {
        Some((_, path)) => BodyRead::File(path),
        None => BodyRead::Bytes(buf),
    }
}

#[allow(clippy::too_many_arguments)]
fn write_response<W: Write>(
    stream: &mut W,
    status: u16,
    body: ResponseBody,
    extra: &[(String, String)],
    close: bool,
    cors: Option<&str>,
    write_body: bool,
    hsts: bool,
) -> io::Result<()> {
    let (ct, payload) = match body {
        ResponseBody::Json(j) => ("application/json".to_string(), dumps(&j).into_bytes()),
        ResponseBody::Raw(r) => (r.content_type, r.body),
    };
    let mut headers = vec![
        ("Content-Type".to_string(), ct),
        ("Content-Length".to_string(), payload.len().to_string()),
    ];
    if close {
        headers.push(("Connection".to_string(), "close".to_string()));
    }
    if let Some(o) = cors {
        headers.push(("Access-Control-Allow-Origin".to_string(), o.to_string()));
    }
    if hsts {
        headers.push((
            "Strict-Transport-Security".to_string(),
            "max-age=31536000; includeSubDomains".to_string(),
        ));
    }
    for (k, v) in extra {
        headers.push((k.clone(), v.clone()));
    }
    send_head(stream, status, &headers)?;
    if write_body {
        stream.write_all(&payload)?;
    }
    stream.flush()
}

fn handle_options<W: Write>(
    rt: &ServeRuntime,
    stream: &mut W,
    path: &str,
) -> io::Result<()> {
    let allowed = rt.methods_for_path(path);
    if allowed.is_empty() {
        return write_response(
            stream,
            404,
            ResponseBody::Json(obj(vec![
                ("error", Json::Str(format!("no route for {}", path))),
                ("status", Json::Int(404)),
            ])),
            &[],
            false,
            rt.cors_origin(),
            true,
            rt.tls_enabled,
        );
    }
    let mut set = allowed;
    set.push("OPTIONS".to_string());
    set.push("HEAD".to_string());
    set.sort();
    set.dedup();
    let allow = set.join(", ");
    let mut headers = vec![
        ("Allow".to_string(), allow.clone()),
        ("Content-Length".to_string(), "0".to_string()),
    ];
    if let Some(o) = rt.cors_origin() {
        headers.push(("Access-Control-Allow-Origin".to_string(), o.to_string()));
        headers.push(("Access-Control-Allow-Methods".to_string(), allow.clone()));
        headers.push(("Access-Control-Allow-Headers".to_string(), "Content-Type, Authorization".to_string()));
        headers.push(("Access-Control-Max-Age".to_string(), "86400".to_string()));
    }
    if rt.tls_enabled {
        headers.push((
            "Strict-Transport-Security".to_string(),
            "max-age=31536000; includeSubDomains".to_string(),
        ));
    }
    send_head(stream, 204, &headers)?;
    stream.flush()
}

/// Emisor SSE: comparte el writer (`Rc<RefCell<S>>`), formatea
/// `[event: <e>\n]data: <json>\n\n` y escribe+flush. Falla con `StreamGone` si el
/// cliente se desconectó. Genérico sobre el transporte (TCP o TLS).
fn make_emitter<S: Write + 'static>(writer: Rc<RefCell<S>>) -> Emitter {
    Box::new(move |value: &SynValue, event: Option<&str>| -> Result<(), StreamGone> {
        let mut payload = String::new();
        if let Some(e) = event {
            payload.push_str("event: ");
            payload.push_str(e);
            payload.push('\n');
        }
        payload.push_str("data: ");
        payload.push_str(&dumps(&syn_to_json(value)));
        payload.push_str("\n\n");
        let mut w = writer.borrow_mut();
        w.write_all(payload.as_bytes()).map_err(|_| StreamGone)?;
        w.flush().map_err(|_| StreamGone)?;
        Ok(())
    })
}

fn write_stream<S: Write + 'static>(
    rt: &ServeRuntime,
    stream: S,
    stream_handler: Option<StreamHandler>,
    ctx: &Ctx,
    write_body: bool,
    hsts: bool,
) -> io::Result<()> {
    let shared = Rc::new(RefCell::new(stream));
    let mut headers = vec![
        ("Content-Type".to_string(), "text/event-stream".to_string()),
        ("Cache-Control".to_string(), "no-cache".to_string()),
        ("X-Accel-Buffering".to_string(), "no".to_string()),
        ("Connection".to_string(), "close".to_string()),
    ];
    if let Some(o) = rt.cors_origin() {
        headers.push(("Access-Control-Allow-Origin".to_string(), o.to_string()));
    }
    if hsts {
        headers.push((
            "Strict-Transport-Security".to_string(),
            "max-age=31536000; includeSubDomains".to_string(),
        ));
    }
    send_head(&mut *shared.borrow_mut(), 200, &headers)?;
    if write_body {
        if let Some(sh) = stream_handler {
            let emit = make_emitter(shared.clone());
            // Headers ya enviados; si el handler falla (no por desconexión) se emite
            // un evento de error best-effort (no se puede cambiar el status).
            if let StreamEnd::Error(msg) = sh(ctx, emit) {
                let err = format!(
                    "event: error\ndata: {}\n\n",
                    dumps(&obj(vec![("error", Json::Str(msg))]))
                );
                let mut w = shared.borrow_mut();
                let _ = w.write_all(err.as_bytes());
                let _ = w.flush();
            }
        }
    }
    rt.release_stream();
    let _ = shared.borrow_mut().flush();
    Ok(())
}

fn handle_connection<S: Read + Write + 'static>(
    rt: Arc<ServeRuntime>,
    stream: S,
    client_ip: String,
) -> io::Result<()> {
    let hsts = rt.tls_enabled;
    let mut reader = BufReader::new(stream);

    loop {
        let (method, target, version, headers) = match read_request_head(&mut reader) {
            Some(x) => x,
            None => break,
        };
        let conn = header_value(&headers, "connection").to_lowercase();
        let close = if version == "HTTP/1.0" {
            conn != "keep-alive"
        } else {
            conn == "close"
        };

        let (path, query) = parse_path_query(&target);

        if method == "OPTIONS" {
            handle_options(&rt, reader.get_mut(), &path)?;
            if close {
                break;
            }
            continue;
        }

        // HEAD = GET sin body.
        let (eff_method, write_body) = if method == "HEAD" {
            ("GET".to_string(), false)
        } else {
            (method.clone(), true)
        };

        let (body_str, body_file): (String, Option<PathBuf>) =
            match read_body(&mut reader, &headers, rt.max_body) {
                BodyRead::TooLarge => {
                    // 413 + cerrar: no dejar un body sin leer en una conexión keep-alive.
                    write_response(
                        reader.get_mut(),
                        413,
                        ResponseBody::Json(obj(vec![
                            ("error", Json::Str("payload too large".into())),
                            ("status", Json::Int(413)),
                        ])),
                        &[],
                        true,
                        rt.cors_origin(),
                        write_body,
                        hsts,
                    )?;
                    break;
                }
                BodyRead::Bytes(b) => (String::from_utf8_lossy(&b).into_owned(), None),
                // Body grande spilled: body vacío en memoria; read_body() lo lee del file.
                BodyRead::File(p) => (String::new(), Some(p)),
            };
        let bf = body_file.as_ref().map(|p| p.to_string_lossy().into_owned());

        match rt.dispatch(&eff_method, &path, query, headers, &body_str, bf.as_deref(), &client_ip) {
            Dispatched::Response { status, body, headers: extra } => {
                write_response(reader.get_mut(), status, body, &extra, close, rt.cors_origin(), write_body, hsts)?;
            }
            Dispatched::Stream { stream_handler, ctx } => {
                let stream = reader.into_inner();
                write_stream(&rt, stream, stream_handler, &ctx, write_body, hsts)?;
                if let Some(p) = &body_file {
                    let _ = std::fs::remove_file(p);
                }
                break; // MVP: un stream por conexión, luego cierra
            }
        }

        if let Some(p) = &body_file {
            let _ = std::fs::remove_file(p);
        }
        if close {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specificity_ordering() {
        // estático < :param < catchall, por segmento.
        assert_eq!(specificity("/products"), vec![0]);
        assert_eq!(specificity("/products/:id"), vec![0, 1]);
        assert_eq!(specificity("/files/*path"), vec![0, 2]);
        assert_eq!(specificity("/"), Vec::<i32>::new());
        // El más específico ordena primero (lista ascendente).
        let mut routes = vec!["/a/*x", "/a/:id", "/a/b"];
        routes.sort_by_key(|p| specificity(p));
        assert_eq!(routes, vec!["/a/b", "/a/:id", "/a/*x"]);
    }

    #[test]
    fn path_match_exact_and_params() {
        assert!(path_match("/health", "/health").is_some());
        assert!(path_match("/health", "/other").is_none());

        let p = path_match("/products/:id", "/products/42").unwrap();
        assert_eq!(p.get("id").map(String::as_str), Some("42"));

        // longitudes distintas → no matchea
        assert!(path_match("/products/:id", "/products/42/extra").is_none());
        assert!(path_match("/products/:id", "/products").is_none());
    }

    #[test]
    fn path_match_catchall() {
        let p = path_match("/files/*path", "/files/a/b/c.txt").unwrap();
        assert_eq!(p.get("path").map(String::as_str), Some("a/b/c.txt"));
        // catchall necesita al menos un segmento
        assert!(path_match("/files/*path", "/files").is_none());
    }

    #[test]
    fn path_match_url_decodes_params() {
        let p = path_match("/u/:name", "/u/jos%C3%A9").unwrap();
        assert_eq!(p.get("name").map(String::as_str), Some("josé"));
    }

    #[test]
    fn param_last_segment_detection() {
        assert!(param_last_segment("/blog/:slug"));
        assert!(!param_last_segment("/blog/post"));
        assert!(!param_last_segment("/files/*path"));
    }

    #[test]
    fn split_format_suffix_works() {
        assert_eq!(split_format_suffix("/blog/hola.json"), ("/blog/hola".into(), Some("json".into())));
        assert_eq!(split_format_suffix("/a.md"), ("/a".into(), Some("md".into())));
        assert_eq!(split_format_suffix("/plain"), ("/plain".into(), None));
        // un sufijo solo (".json") sin nombre no cuenta
        assert_eq!(split_format_suffix("/.json"), ("/.json".into(), None));
        // un sufijo tras '/' no cuenta
        assert_eq!(split_format_suffix("/dir/.md"), ("/dir/.md".into(), None));
    }

    #[test]
    fn negotiate_format_defaults_html() {
        assert_eq!(negotiate_format(""), "html");
        assert_eq!(negotiate_format("*/*"), "html");
        assert_eq!(negotiate_format("text/html"), "html");
        assert_eq!(negotiate_format("application/json"), "json");
        assert_eq!(negotiate_format("text/markdown"), "md");
        // html gana si está presente junto a otro
        assert_eq!(negotiate_format("application/json, text/html"), "html");
    }

    #[test]
    fn parse_body_size_units() {
        assert_eq!(parse_body_size_str("512kb"), Some(512 * 1024));
        assert_eq!(parse_body_size_str("10mb"), Some(10 * 1024 * 1024));
        assert_eq!(parse_body_size_str("1gb"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_body_size_str("2048"), Some(2048));
        assert_eq!(parse_body_size_str("unlimited"), None);
        assert_eq!(parse_body_size_str("none"), None);
        assert_eq!(parse_body_size_str("garbage"), Some(MAX_BODY));
    }

    #[test]
    fn parse_path_query_basic() {
        let (p, q) = parse_path_query("/search?q=hello+world&limit=10");
        assert_eq!(p, "/search");
        assert_eq!(q.get("q").map(String::as_str), Some("hello world"));
        assert_eq!(q.get("limit").map(String::as_str), Some("10"));
        let (p2, q2) = parse_path_query("/plain");
        assert_eq!(p2, "/plain");
        assert!(q2.is_empty());
    }

    // ===================== A2: estáticos de producción =====================

    #[test]
    fn parse_range_forms() {
        // bytes=START-END inclusivos
        assert_eq!(parse_range("bytes=0-3", 10), Some((0, 3)));
        // bytes=START- → hasta el final
        assert_eq!(parse_range("bytes=5-", 10), Some((5, 9)));
        // bytes=-N → últimos N
        assert_eq!(parse_range("bytes=-3", 10), Some((7, 9)));
        // END recortado a size-1
        assert_eq!(parse_range("bytes=2-999", 10), Some((2, 9)));
        // start fuera de rango → None (416)
        assert_eq!(parse_range("bytes=10-12", 10), None);
        assert_eq!(parse_range("bytes=20-", 10), None);
        // start > end → None
        assert_eq!(parse_range("bytes=5-2", 10), None);
        // size 0 → None
        assert_eq!(parse_range("bytes=0-0", 0), None);
        // sin prefijo bytes= → None
        assert_eq!(parse_range("0-3", 10), None);
    }

    #[test]
    fn compressible_types() {
        assert!(is_compressible("text/html"));
        assert!(is_compressible("text/html; charset=utf-8"));
        assert!(is_compressible("text/css"));
        assert!(is_compressible("application/json"));
        assert!(is_compressible("image/svg+xml"));
        assert!(is_compressible("text/plain"));
        assert!(!is_compressible("image/png"));
        assert!(!is_compressible("application/octet-stream"));
        assert!(!is_compressible("font/woff2"));
    }

    #[test]
    fn gzip_roundtrip() {
        let data = b"hola mundo, esto se comprime bien bien bien bien bien".repeat(10);
        let gz = gzip_bytes(&data).expect("gzip");
        // El header gzip arranca con 0x1f 0x8b.
        assert_eq!(&gz[..2], &[0x1f, 0x8b]);
        let mut dec = flate2::read::GzDecoder::new(&gz[..]);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    /// Construye un ServeRuntime mínimo con un único static mount sobre `dir`.
    fn static_rt(dir: &str) -> ServeRuntime {
        ServeRuntime::new(
            0,
            "0.0.0.0".to_string(),
            Vec::new(),
            None,
            None,
            64,
            vec![("/".to_string(), dir.to_string())],
            None,
            None,
            None,
            Vec::new(),
            false,
            false,
        )
    }

    #[test]
    fn serve_static_etag_range_gzip() {
        // dir temporal único para este test
        let dir = std::env::temp_dir().join("syn_static_a2_test");
        let _ = std::fs::create_dir_all(&dir);
        let body = b"<!doctype html><h1>hola</h1> contenido suficiente para comprimir".to_vec();
        std::fs::write(dir.join("index.html"), &body).unwrap();
        let rt = static_rt(&dir.to_string_lossy());

        // (1) GET normal → 200 + ETag + body completo.
        let r = rt.default_host.serve_static_full("/index.html", &[]).expect("estático");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, body);
        let etag = r
            .extra
            .iter()
            .find(|(k, _)| k == "ETag")
            .map(|(_, v)| v.clone())
            .expect("ETag");
        assert!(etag.starts_with('"') && etag.ends_with('"'));

        // (2) If-None-Match con ese etag → 304 sin body.
        let h304 = vec![("If-None-Match".to_string(), etag.clone())];
        let r = rt.default_host.serve_static_full("/index.html", &h304).expect("estático");
        assert_eq!(r.status, 304);
        assert!(r.body.is_empty());

        // (3) Range bytes=0-3 → 206 + Content-Range + 4 bytes.
        let hr = vec![("Range".to_string(), "bytes=0-3".to_string())];
        let r = rt.default_host.serve_static_full("/index.html", &hr).expect("estático");
        assert_eq!(r.status, 206);
        assert_eq!(r.body, &body[0..=3]);
        let cr = r.extra.iter().find(|(k, _)| k == "Content-Range").map(|(_, v)| v.clone());
        assert_eq!(cr, Some(format!("bytes 0-3/{}", body.len())));

        // (4) Accept-Encoding: gzip sobre text/html → Content-Encoding gzip + Vary.
        let hg = vec![("Accept-Encoding".to_string(), "gzip, deflate".to_string())];
        let r = rt.default_host.serve_static_full("/index.html", &hg).expect("estático");
        assert_eq!(r.status, 200);
        assert!(r
            .extra
            .iter()
            .any(|(k, v)| k == "Content-Encoding" && v == "gzip"));
        assert!(r.extra.iter().any(|(k, v)| k == "Vary" && v == "Accept-Encoding"));
        let mut dec = flate2::read::GzDecoder::new(&r.body[..]);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        assert_eq!(out, body);

        // (5) path inexistente → None.
        assert!(rt.default_host.serve_static_full("/nope.html", &[]).is_none());
    }
}

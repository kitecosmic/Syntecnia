"""
Syntecnia templates — ergonomic SSR for design pages (landing, marketing).

A template is HTML with `{ ... }` holes:

    <h1>{ title }</h1>                          interpolation (AUTO-ESCAPED)
    { each item in items }<li>{ item }</li>{ end }   loop (reuses Syntecnia `each`)
    { when featured }<aside>*</aside>{ end }    conditional (reuses Syntecnia `when`)
    { raw trusted_html }                        opt out of escaping

Security (the point): every interpolated value is HTML-escaped by default, so a
template can't be an XSS hole — the author never has to remember. `raw(expr)`
opts out for trusted HTML.

Flow control inside `{}` reuses Syntecnia's own `each`/`when`/`otherwise`
expressions — it is not a new dialect. Template paths are resolved relative to
the working directory (escaping it is blocked). Errors carry file:line.

Note: `{` and `}` are template delimiters. Put CSS/JS (which use braces) in
external files served via `static`; to emit a literal brace use a string hole
like `{ "{" }`.
"""

import html as _html
import os

from ..core.types import SynValue, SynText, SynList, SynMap


class TemplateError(RuntimeError):
    """A template parse/render error. Carries file:line context where possible."""


# =========================================================
# Path resolution (cwd-scoped, traversal-safe)
# =========================================================

def resolve_template_path(path: str) -> str:
    cwd = os.path.realpath(os.getcwd())
    if os.path.isabs(path):
        raise TemplateError(f"template path must be relative to the working dir: {path!r}")
    target = os.path.realpath(os.path.join(cwd, path))
    if target != cwd and not target.startswith(cwd + os.sep):
        raise TemplateError(f"template path escapes the working directory: {path!r}")
    if not os.path.isfile(target):
        raise TemplateError(f"template not found: {path}")
    return target


# =========================================================
# Splitting source into text / hole segments (quote-aware)
# =========================================================

def _segments(src: str, filename: str):
    """Yield ('text', s) and ('hole', content, line) segments."""
    segs = []
    i, n, line = 0, len(src), 1
    buf = []
    while i < n:
        c = src[i]
        if c == "{":
            if buf:
                segs.append(("text", "".join(buf)))
                buf = []
            hole_line = line
            j = i + 1
            in_str = esc = False
            content = []
            while j < n:
                cj = src[j]
                if cj == "\n":
                    line += 1
                if in_str:
                    content.append(cj)
                    if esc:
                        esc = False
                    elif cj == "\\":
                        esc = True
                    elif cj == '"':
                        in_str = False
                elif cj == '"':
                    in_str = True
                    content.append(cj)
                elif cj == "}":
                    break
                else:
                    content.append(cj)
                j += 1
            if j >= n:
                raise TemplateError(f"{filename}:{hole_line}: unclosed '{{' in template")
            segs.append(("hole", "".join(content).strip(), hole_line))
            i = j + 1
        else:
            if c == "\n":
                line += 1
            buf.append(c)
            i += 1
    if buf:
        segs.append(("text", "".join(buf)))
    return segs


# =========================================================
# Parsing holes (expressions + each/when directives)
# =========================================================

def _parse_expr(expr_src: str, filename: str, line: int):
    from ..core.lexer import Lexer
    from ..core.parser import Parser
    try:
        tokens = Lexer(expr_src, filename).tokenize_filtered()
        return Parser(tokens, filename)._parse_expression()
    except TemplateError:
        raise
    except Exception as e:
        raise TemplateError(
            f"{filename}:{line}: invalid expression {{ {expr_src} }}: {e}")


def _parse_each(content: str, filename: str, line: int):
    """Parse 'item in collection' → (var_name, collection_expr_ast)."""
    from ..core.lexer import Lexer
    from ..core.parser import Parser
    from ..core.tokens import TokenType
    try:
        tokens = Lexer(content, filename).tokenize_filtered()
        p = Parser(tokens, filename)
        var = p._current()
        if var.type != TokenType.IDENTIFIER:
            raise TemplateError(
                f"{filename}:{line}: expected a loop variable in '{{ each {content} }}'")
        p._advance()
        if p._current().type != TokenType.IN:
            raise TemplateError(
                f"{filename}:{line}: expected 'in' in '{{ each {content} }}'")
        p._advance()
        coll = p._parse_expression()
        return var.value, coll
    except TemplateError:
        raise
    except Exception as e:
        raise TemplateError(f"{filename}:{line}: invalid 'each' directive: {e}")


def _build_tree(segs, filename: str):
    """Build a nested node tree from flat segments, honouring each/when/end."""
    root = []
    stack = [{"list": root, "node": None, "kind": "root", "line": 0}]

    for seg in segs:
        target = stack[-1]["list"]
        if seg[0] == "text":
            target.append({"t": "text", "s": seg[1]})
            continue
        content, line = seg[1], seg[2]
        parts = content.split(None, 1)
        head = parts[0] if parts else ""
        rest = parts[1] if len(parts) > 1 else ""

        if head == "each":
            var, coll = _parse_each(rest, filename, line)
            node = {"t": "each", "var": var, "coll": coll, "body": [], "line": line}
            target.append(node)
            stack.append({"list": node["body"], "node": node, "kind": "each", "line": line})
        elif head == "when":
            cond = _parse_expr(rest, filename, line)
            node = {"t": "when", "cond": cond, "then": [], "else": [], "line": line}
            target.append(node)
            stack.append({"list": node["then"], "node": node, "kind": "when", "line": line})
        elif head == "otherwise":
            top = stack[-1]
            if top["kind"] != "when":
                raise TemplateError(
                    f"{filename}:{line}: 'otherwise' outside a 'when' block")
            top["list"] = top["node"]["else"]
        elif head == "end":
            if len(stack) <= 1:
                raise TemplateError(
                    f"{filename}:{line}: '{{ end }}' without a matching block")
            stack.pop()
        elif head == "raw":
            target.append({"t": "expr", "e": _parse_expr(rest, filename, line),
                           "escape": False, "line": line})
        else:
            target.append({"t": "expr", "e": _parse_expr(content, filename, line),
                           "escape": True, "line": line})

    if len(stack) > 1:
        open_block = stack[-1]
        raise TemplateError(
            f"{filename}:{open_block['line']}: missing '{{ end }}' for "
            f"'{{ {open_block['kind']} ... }}'")
    return root


# =========================================================
# Rendering
# =========================================================

def _display(value: SynValue) -> str:
    if isinstance(getattr(value, "type", None), SynText):
        return value.raw
    return str(value)


def _render_nodes(nodes, interp, env, out):
    from ..core.interpreter import Environment
    for node in nodes:
        t = node["t"]
        if t == "text":
            out.append(node["s"])
        elif t == "expr":
            val = interp._exec(node["e"], env)
            s = _display(val)
            out.append(s if not node["escape"] else _html.escape(s, quote=True))
        elif t == "each":
            coll = interp._exec(node["coll"], env)
            items = coll.raw if isinstance(coll.type, SynList) else []
            for item in items:
                child = Environment(parent=env, name="template:each")
                child.set(node["var"], item)
                _render_nodes(node["body"], interp, child, out)
        elif t == "when":
            cond = interp._exec(node["cond"], env)
            branch = node["then"] if cond.is_truthy() else node["else"]
            _render_nodes(branch, interp, env, out)


def render_template(interp, path: str, data) -> str:
    """Render template `path` with `data` (a SynMap) bound as variables → HTML."""
    from ..core.interpreter import Environment
    target = resolve_template_path(path)
    filename = os.path.basename(target)
    with open(target, "r", encoding="utf-8") as f:
        src = f.read()
    tree = _build_tree(_segments(src, filename), filename)
    env = Environment(parent=interp.global_env, name=f"template:{filename}")
    if data is not None and isinstance(data.type, SynMap):
        for k, v in data.raw.items():
            env.set(str(k), v)
    out = []
    _render_nodes(tree, interp, env, out)
    return "".join(out)

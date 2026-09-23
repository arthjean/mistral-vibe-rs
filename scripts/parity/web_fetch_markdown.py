#!/usr/bin/env python3
"""Capture the Markdown the pinned Python reference makes of an HTML page.

Reference ``web_fetch`` hands a ``text/html`` body to ``_html_to_markdown``
(``vibe/core/tools/builtins/web_fetch.py``), which runs ``markdownify`` over
Beautiful Soup's ``html.parser`` tree. This script calls that function directly
over inputs it authors and records what comes back, or the message of what it
raises, so the Rust replay in
``crates/vibe-core/src/tools/builtins/web_fetch/markdown_tests.rs`` can compare
the port page by page.

Two input families are recorded. ``fixtures`` are hand-written pages, each
aimed at one rule of the tokenizer, the tree builder, or a converter.
``generated`` are pages a seeded generator composes from the same vocabulary,
nesting tags, attributes, references, comments and whitespace in combinations
nobody would think to write; the corpus stores each page itself, so the replay
never has to reproduce the generator.

Every input is authored here and every output is what the library produced
from it, so the corpus holds observations and no reference-authored text.

Usage::

    scripts/parity/web_fetch_markdown.py --reference /path/to/reference

``VIBE_REFERENCE`` sets the checkout for machines that do not hold it at the
default path; ``--reference`` wins over it. The wrapper re-executes itself with
the reference interpreter when the current one cannot import ``vibe``.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import random
import subprocess
import sys
from typing import Any

#: The pin and the checkout path come from the one place this repository writes
#: them, so a re-pin does not have to find this script.
from pin import DEFAULT_REFERENCE, EXPECTED_COMMIT

SCHEMA_VERSION = 1
DEFAULT_OUTPUT = Path("crates/vibe-core/tests/web-fetch-markdown/corpus.json")
INTERPRETER_VARIABLE = "VIBE_PARITY_PYTHON"
GENERATED_SEED = 20260923
GENERATED_COUNT = 400


class OracleError(RuntimeError):
    """Raised when the corpus cannot be produced from an authoritative state."""


def resolve_reference(reference: Path, expected_commit: str | None) -> dict[str, str]:
    if not reference.is_dir():
        raise OracleError(f"reference checkout is missing: {reference}")
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=reference,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise OracleError(
            f"git rev-parse failed in {reference}: {result.stderr.strip()}"
        )
    commit = result.stdout.strip()
    if expected_commit and commit != expected_commit:
        raise OracleError(
            f"reference checkout is at {commit}, not the pinned {expected_commit}"
        )
    return {"commit": commit}


def reexecute_with_reference_interpreter(
    reference: Path, interpreter: Path | None
) -> None:
    """Re-runs this script under an interpreter that can import ``vibe``."""
    try:
        import vibe  # noqa: F401

        return
    except ImportError:
        pass
    candidates = [
        interpreter,
        Path(os.environ[INTERPRETER_VARIABLE])
        if os.environ.get(INTERPRETER_VARIABLE)
        else None,
        reference / ".venv/bin/python",
        reference / ".venv/Scripts/python.exe",
    ]
    candidate = next(
        (path for path in candidates if path is not None and path.is_file()), None
    )
    if candidate is None:
        raise OracleError(
            f"cannot import `vibe` and no reference interpreter under {reference}"
        )
    if Path(sys.executable).resolve() == candidate.resolve():
        raise OracleError(f"{candidate} cannot import `vibe`")
    os.execv(
        str(candidate), [str(candidate), str(Path(__file__).resolve()), *sys.argv[1:]]
    )


# --------------------------------------------------------------------------
# Inputs
# --------------------------------------------------------------------------

#: Hand-written pages, one rule each. The keys name the rule.
FIXTURES: dict[str, str] = {
    "empty": "",
    "plain-text": "just words",
    "whitespace-only": " \n\t ",
    "document": (
        "<!DOCTYPE html><html><head><title>Garden log</title>"
        "<style>p{color:red}</style><script>var a = '<p>no</p>';</script></head>"
        "<body><h1>Spring beds</h1><p>Turn the soil &amp; add compost.</p>"
        "<noscript>enable scripts</noscript><iframe src='x'>frame</iframe>"
        "<object>obj</object><embed src='y'><p>Water <em>daily</em>.</p></body></html>"
    ),
    "headings": "<h1>One</h1><h2>Two  words</h2><h3>\nThree\n</h3><h6>Six</h6><h7>Seven</h7><h10>Ten</h10><h0>Zero</h0>",
    "heading-inline": "<h2>Title <p>para</p> <div>block</div> <br> <img alt='pic' src='a.png'></h2>",
    "heading-nested": "<h1>outer <h2>inner</h2> tail</h1>",
    "emphasis": "<p><b>bold</b> <strong> strong </strong> <i>it</i> <em>em</em> <del>gone</del> <s>struck</s> <sub>2</sub><sup>3</sup></p>",
    "emphasis-empty": "<p>a<b> </b>b<em></em>c</p>",
    "escape": "<p>2*3 = snake_case * _x_</p><pre>raw * _kept_</pre><code>a*b_c</code>",
    "links": (
        "<p><a href='https://example.test/a'>site</a> "
        "<a href='https://example.test/b' title='The \"b\" page'>titled</a> "
        "<a href='https://example.test/c'>https://example.test/c</a> "
        "<a href='https://example.test/snake_case'>https://example.test/snake_case</a> "
        "<a>no href</a> <a href=''>empty href</a> <a href='x'> spaced </a> <a href='y'></a></p>"
    ),
    "images": "<p><img src='a.png' alt='An image' title='Say \"hi\"'> <img src='b.png'> <img alt='only alt'></p>",
    "video": (
        "<video src='v.mp4' poster='p.png'>clip</video>"
        "<video poster='p.png'>poster only</video>"
        "<video><source src='s.webm'><source src='t.mp4'>sourced</video>"
        "<video>bare</video><h3><video src='v.mp4'>inline</video></h3>"
    ),
    "unordered": "<ul><li>one</li><li>two\n<ul><li>nested</li><li>deeper<ul><li>deepest</li></ul></li></ul></li><li></li></ul><p>after</p>",
    "ordered": "<ol><li>first</li><li>second</li></ol><ol start='7'><li>seven</li><li>eight</li></ol><ol start='x'><li>bad</li></ol><ol start=''><li>blank</li></ol><ol start='007'><li>zeros</li></ol>",
    "ordered-unicode": "<ol start='٣'><li>arabic three</li></ol>",
    "ordered-invalid": "<ol start='½'><li>half</li></ol>",
    "list-then-list": "<ul><li>a</li></ul><ol><li>b</li></ol><ul><li>c</li></ul>",
    "list-item-multiline": "<ul><li><p>para one</p><p>para two</p></li><li>line<br>break</li></ul>",
    "list-mixed-children": "<ol><li>a</li><p>stray</p><li>b</li></ol>",
    "blockquote": "<blockquote>quoted\n<p>para</p><p>second</p></blockquote><blockquote> </blockquote><blockquote><blockquote>nested</blockquote></blockquote>",
    "code": "<p>Use <code>ls -la</code> or <code>a`b</code> or <code>``x``</code> and <kbd>Ctrl</kbd> <samp>out</samp> <code> </code></p>",
    "pre": "<pre>\n\n  indented\n    more\n\n</pre><pre></pre><pre><code>fn main() {}\n</code></pre><pre>a <b>bold</b> _u_</pre>",
    "pre-nested-whitespace": "<pre>  \n  lead</pre><div><pre>x</pre> <pre>y</pre></div>",
    "textarea": "<textarea>  keep\n  this  </textarea><p>after</p>",
    "breaks": "<p>one<br>two<br/>three<br></br>four</p><td>cell<br>line</td>",
    "horizontal-rule": "<p>above</p><hr><p>below</p><hr/>",
    "divisions": "<div>one</div><div> </div><article>two</article><section>three</section><div><div>nested</div></div>",
    "definition-list": "<dl><dt>Term\n one</dt><dd>Definition\nline two</dd><dt></dt><dd></dd><dd><p>para</p></dd></dl>",
    "quote": "<p>He said <q>hello</q>.</p>",
    "table-headed": "<table><tr><th>Name</th><th>Qty</th></tr><tr><td>Apple</td><td>3</td></tr></table>",
    "table-bare": "<table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>",
    "table-sections": (
        "<table><caption>Stock</caption><thead><tr><th>Item</th><th>Count</th></tr></thead>"
        "<tbody><tr><td>Pears</td><td>4</td></tr><tr><td>Figs</td><td>9</td></tr></tbody></table>"
    ),
    "table-tbody-only": "<table><tbody><tr><td>x</td></tr><tr><td>y</td></tr></tbody></table>",
    "table-thead-two-rows": "<table><thead><tr><td>h1</td></tr><tr><td>h2</td></tr></thead><tbody><tr><td>b</td></tr></tbody></table>",
    "table-colspan": "<table><tr><th colspan='2'>Wide</th><th colspan='0'>zero</th></tr><tr><td colspan='3'>all</td><td colspan='x'>bad</td></tr></table>",
    "table-colspan-huge": "<table><tr><td colspan='99999'>w</td></tr></table>",
    "table-colspan-invalid": "<table><tr><td colspan='²'>sq</td></tr></table>",
    "table-cell-content": "<table><tr><td><p>para</p><ul><li>item</li></ul></td><td>line\nbreak</td></tr></table>",
    "table-second-tbody": "<table><tbody><tr><td>a</td></tr></tbody><tbody><tr><td>b</td></tr></tbody></table>",
    "figure": "<figure><img src='f.png' alt='fig'><figcaption>Caption <b>bold</b></figcaption></figure>",
    "entities": (
        "<p>&amp; &lt; &gt; &quot; &apos; &nbsp;x &copy; &eacute; &#65; &#x42; &#x1F600; "
        "&#0; &#128; &#x80; &#xD800; &#1114112; &notanentity; &amp &copy2 &ampx; &#; &#x; "
        "&notin; &notit; AT&T</p>"
    ),
    "entities-attribute": "<a href='?a=1&amp;b=2&copy=3&lang=en'>q</a><img alt='&lt;x&gt;' src='i.png'>",
    "comments": "<p>a<!-- hidden -->b<!---->c</p><!-- top --><pre>d<!---->e</pre><p>f<!->g</p>",
    "bogus-comments": "<p>a<!x>b</p><p>c</? odd>d</p><p>e<?php echo 1 ?>f</p>",
    "cdata": "<p>a<![CDATA[ <inner> ]]>b</p><p>c<![CDATA[]]>d</p>",
    "declarations": "<!DOCTYPE html><!ELEMENT br EMPTY><p>x</p>",
    "unclosed": "<p>one<p>two<div>three",
    "stray-end-tags": "</p><p>a</b>b</div>c</p></span>",
    "misnested": "<b><i>both</b> italic?</i> plain",
    "void-closed": "<br></br><img src='a.png'></img><p>after</p><input>text</input>",
    "self-closing-non-void": "<div/>text<span/>more<p/>para",
    "uppercase": "<P>Upper <B>Bold</B> <A HREF='u'>Link</A></P><UL><LI>Item</LI></UL>",
    "attributes": "<a href=unquoted title=\"double\" data-x='single' disabled>attrs</a><a href='x' href='y'>dupe</a><a href>novalue</a>",
    "attribute-edge": "<a href = 'spaced' title=\"a>b\">gt in value</a><img src=\"q\"alt=\"tight\"><p class=\"a b\">classes</p>",
    "script-content": "<script>if (a < b && c > d) { document.write('</div>'); }</script><p>visible</p>",
    "script-unclosed": "<p>before</p><script>never closed <p>x</p>",
    "style-uppercase-end": "<STYLE>p { } </STYLE><p>shown</p>",
    "raw-text-elements": "<xmp><b>kept</b></xmp><noembed>ne</noembed><noframes>nf</noframes><title>t &amp; t</title>",
    "plaintext": "<p>x</p><plaintext><b>all raw</b></plaintext>tail",
    "whitespace-inline": "<p>a  b\t\tc\n\nd</p><span>  lead</span><span>trail  </span> <b> x </b>",
    "whitespace-blocks": "<div>\n  <p>\n  para\n  </p>\n  <p>next</p>\n</div>\n\n<p>  after  </p>",
    "unicode-spaces": "<p> nbsp </p><p> em space </p><p>　ideographic</p>",
    "newlines-collapse": "<p>a</p>\n\n\n<p>b</p><br><br><br><p>c</p>",
    "custom-elements": "<my-widget>custom</my-widget><x:tag>ns</x:tag><fn-cache>fc</fn-cache>",
    "soup-element": "<p>x</p><soup>broth</soup>",
    "fn-cache-later": "<b>x</b><fn-cache>boom</fn-cache>",
    "fn-cache-first": "<fn-cache>first</fn-cache><b>then</b><fn-cache>again</fn-cache>",
    "list-element": "<list><li>listed</li></list>",
    "svg-math": "<svg><text>drawn</text></svg><math><mi>x</mi></math>",
    "form": "<form><label>Name</label><input value='v'><select><option>o</option></select><button>Go</button></form>",
    "inline-in-blocks": "<div><span>a</span> <span>b</span></div><div>x <div>y</div> z</div>",
    "control-characters": "<p>bell\u0007 nul\u0000 esc\u001b sep\u001c</p>",
    "crlf": "<p>one\r\ntwo\rthree</p>",
    "deep-nesting-ok": "<div>" * 480 + "deep" + "</div>" * 480,
    "deep-nesting-too-deep": "<div>" * 600 + "deeper" + "</div>" * 600,
    "lt-in-text": "<p>1 < 2 and 3 > 2, a <b x</p><p>tail <</p>",
    "incomplete-tag-at-end": "<p>text</p><div class=",
}

#: The vocabulary the generator composes pages from.
GENERATED_TAGS = [
    "p", "div", "span", "b", "strong", "i", "em", "a", "ul", "ol", "li", "h1", "h2",
    "h3", "h5", "pre", "code", "blockquote", "table", "thead", "tbody", "tr", "td",
    "th", "br", "hr", "img", "dl", "dt", "dd", "q", "del", "sub", "sup", "section",
    "article", "kbd", "figcaption", "caption", "video", "source", "script", "style",
    "noscript", "textarea", "custom-tag",
]
GENERATED_WORDS = [
    "alpha", "beta", "snake_case", "2*3", "AT&T", "&amp;", "&lt;tag&gt;", "&copy;",
    "&#169;", "x`y", "``", "  ", "\n", "\t", " ", "café", "-", "#", "1.",
    "[link]", "a > b", "<", " ", "",
]
GENERATED_ATTRIBUTES = [
    "href='https://example.test/p'", "href='snake_case'", "title='t \"q\"'",
    "src='i.png'", "alt='alt text'", "colspan='2'", "colspan='x'", "start='3'",
    "poster='p.png'", "class='c'", "href", "title=''",
]


def generated_page(rng: random.Random) -> str:
    """One page composed from the vocabulary, with some tags left open or
    closed out of order on purpose."""

    def fragment(depth: int) -> str:
        parts = []
        for _ in range(rng.randint(1, 4)):
            roll = rng.random()
            if depth > 5 or roll < 0.35:
                parts.append(rng.choice(GENERATED_WORDS))
            elif roll < 0.42:
                parts.append(rng.choice(["<!-- c -->", "<!---->", "<![CDATA[cd]]>", "<?pi?>"]))
            else:
                tag = rng.choice(GENERATED_TAGS)
                attributes = " ".join(
                    rng.sample(GENERATED_ATTRIBUTES, rng.randint(0, 2))
                )
                opening = f"<{tag} {attributes}>" if attributes else f"<{tag}>"
                closing = "" if rng.random() < 0.1 else f"</{tag}>"
                parts.append(f"{opening}{fragment(depth + 1)}{closing}")
            if rng.random() < 0.3:
                parts.append(rng.choice([" ", "\n", "\n\n  ", ""]))
        return "".join(parts)

    return fragment(0)


# --------------------------------------------------------------------------
# Capture
# --------------------------------------------------------------------------


def observe(html: str) -> dict[str, str]:
    from vibe.core.tools.builtins.web_fetch import _html_to_markdown

    try:
        return {"markdown": _html_to_markdown(html)}
    except Exception as error:  # the reference reports any failure as the tool error
        return {"error": str(error)}


def build_corpus(reference: Path, expected_commit: str | None) -> dict[str, Any]:
    pin = resolve_reference(reference, expected_commit)
    fixtures = [
        {"name": name, "html": html, **observe(html)}
        for name, html in FIXTURES.items()
    ]
    rng = random.Random(GENERATED_SEED)
    generated = []
    for index in range(GENERATED_COUNT):
        html = generated_page(rng)
        generated.append({"name": f"generated-{index:03}", "html": html, **observe(html)})
    return {
        "schemaVersion": SCHEMA_VERSION,
        "reference": pin,
        "note": (
            "Captured from the pinned reference by scripts/parity/web_fetch_markdown.py. "
            "Every page is authored by the script; every output is what the "
            "reference's _html_to_markdown produced from it."
        ),
        "cases": fixtures + generated,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, default=DEFAULT_REFERENCE)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument(
        "--interpreter",
        type=Path,
        default=None,
        help="Python that can import `vibe`; also read from " + INTERPRETER_VARIABLE,
    )
    parser.add_argument(
        "--allow-unpinned",
        action="store_true",
        help="capture from a checkout at another revision, for a re-pin",
    )
    arguments = parser.parse_args()

    try:
        reexecute_with_reference_interpreter(arguments.reference, arguments.interpreter)
        corpus = build_corpus(
            arguments.reference,
            None if arguments.allow_unpinned else EXPECTED_COMMIT,
        )
    except OracleError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    arguments.output.write_text(
        json.dumps(corpus, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    failures = sum(1 for case in corpus["cases"] if "error" in case)
    print(
        f"wrote {arguments.output} ({len(corpus['cases'])} pages, {failures} raise)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

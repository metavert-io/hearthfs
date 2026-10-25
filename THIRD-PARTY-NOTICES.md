# Third-party notices

HearthFS itself is MIT-licensed (see [LICENSE](LICENSE)). It builds on the
third-party software below. Nothing here is a claim of affiliation.

## C code compiled into the binaries

These Rust crates compile bundled C sources into `hearthfs` and into the
FSKit extension's static library.

### SQLite

Bundled by `libsqlite3-sys` 0.30.1 (through `rusqlite`'s `bundled`
feature): SQLite 3.46.0, https://sqlite.org. SQLite is in the public
domain. Its source carries this blessing in place of a licence:

> May you do good and not evil.
> May you find forgiveness for yourself and forgive others.
> May you share freely, never taking more than you give.

### Oniguruma

Bundled by `onig_sys` 69.9.3 (through `tokenizers`' `onig` feature):
Oniguruma 6.9.10, https://github.com/kkos/oniguruma, under the BSD
2-Clause licence:

```
Copyright (c) 2002-2021  K.Kosako
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:
1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in the
   documentation and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS ``AS IS'' AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
SUCH DAMAGE.
```

### tree-sitter and its grammars

The tree-sitter runtime and nine grammars are compiled in, all under the
MIT licence. The copyright notices are in each project's repository:

| crate | version | upstream |
|---|---|---|
| `tree-sitter` | 0.24.7 | https://github.com/tree-sitter/tree-sitter |
| `tree-sitter-c` | 0.23.4 | https://github.com/tree-sitter/tree-sitter-c |
| `tree-sitter-cpp` | 0.23.4 | https://github.com/tree-sitter/tree-sitter-cpp |
| `tree-sitter-go` | 0.23.4 | https://github.com/tree-sitter/tree-sitter-go |
| `tree-sitter-java` | 0.23.5 | https://github.com/tree-sitter/tree-sitter-java |
| `tree-sitter-javascript` | 0.23.1 | https://github.com/tree-sitter/tree-sitter-javascript |
| `tree-sitter-python` | 0.23.6 | https://github.com/tree-sitter/tree-sitter-python |
| `tree-sitter-ruby` | 0.23.1 | https://github.com/tree-sitter/tree-sitter-ruby |
| `tree-sitter-rust` | 0.23.3 | https://github.com/tree-sitter/tree-sitter-rust |
| `tree-sitter-typescript` | 0.23.2 | https://github.com/tree-sitter/tree-sitter-typescript |

The MIT licence, as it applies to each of them with that project's own
copyright line:

```
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

Other crates that compile bundled C, C++ or assembly (`ring`, `esaxx-rs`,
`tract-linalg`) are covered by their crate licences below.

## Rust crates

The exact crates and versions are pinned in [Cargo.lock](Cargo.lock): 373
third-party crates, counting those that only build for Linux or Windows.
Almost all are MIT and/or Apache-2.0; the others are BSD-3-Clause, ISC,
Unicode-3.0, Unlicense, CDLA-Permissive-2.0 (`webpki-roots`, Mozilla's CA
certificate list), CC0-1.0 and Zlib. [deny.toml](deny.toml) is the
allow-list `cargo deny check licenses` enforces. To print every crate's
licence, run `cargo deny list`; to render the full licence texts, use a
tool such as `cargo about`.

## Embedding model (not distributed)

Local semantic search uses
[sentence-transformers/all-MiniLM-L6-v2](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2),
which is licensed under Apache-2.0. The model is not part of this
repository or its builds: `scripts/fetch-model.sh` downloads `model.onnx`
and `tokenizer.json` from Hugging Face at a pinned revision and checks
their SHA-256 hashes.

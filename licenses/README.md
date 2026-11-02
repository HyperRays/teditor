# Third-party programs and code

## Universal Ctags

`teditor` runs [Universal Ctags](https://ctags.io/) as a separate program to find symbols. It
is licensed under the GNU General Public License, version 2 or later; see `vendor/ctags/COPYING`.
`teditor` never links to it or contains its code, and only reads the tag files it writes.

Its complete source is the `vendor/ctags` submodule, pinned to the exact version built. It is
configured with every optional library disabled, so the binary contains no other licensed
libraries beyond the system's.

Anyone distributing a build that includes the `ctags` binary must also include its licence and
provide that source, for example by publishing a source archive beside each release.

## portable-pty

The Windows pseudo-console code is adapted from WezTerm's `portable-pty`, under the MIT license;
see `portable-pty-MIT.txt`.

## ripgrep's libraries

Text search links the `grep-searcher`, `grep-regex`, `grep-matcher` and `ignore` crates from
[ripgrep](https://github.com/BurntSushi/ripgrep), each available under the MIT license or the Unlicense.

## Syntax highlighting

Highlighting links [syntect](https://github.com/trishume/syntect), under the MIT license, and
[two-face](https://codeberg.org/CosmicHarper/two-face), available under the MIT or Apache 2.0 license,
which embeds the syntax definitions curated by the [bat](https://github.com/sharkdp/bat) project.
Those definitions have their own licenses, collected in `syntax-grammars.md`.

## Nerd Fonts

`assets/fonts/SymbolsNerdFontMono-Regular.ttf` is from [Nerd Fonts](https://github.com/ryanoasis/nerd-fonts)
3.4.0, whose work is under the MIT license; see `assets/fonts/NerdFonts-MIT.txt`. It collects icons from
other projects under their own licenses, which Nerd Fonts lists in its repository. The find bar's two icons are
from Microsoft's [Codicons](https://github.com/microsoft/vscode-codicons).

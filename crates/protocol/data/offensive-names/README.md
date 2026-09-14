# Offensive-name data

`en` is the English list from [LDNOOBW/List-of-Dirty-Naughty-Obscene-and-Otherwise-Bad-Words](https://github.com/LDNOOBW/List-of-Dirty-Naughty-Obscene-and-Otherwise-Bad-Words), pinned at commit `5faf2ba42d7b1c0977169ec3611df25a3c08eb13` (the English file's last change is `4638b970cb8d9d82789564fcba1f4a1eb508ff1a`). Copyright 2012–2020 Shutterstock, Inc.; see `LICENSE` for the CC BY 4.0 license.

Tandem adapts the source at runtime for hosted repository names. It drops entries outside the lowercase ASCII name grammar, compares whole separator-delimited tokens and phrases (including compact spellings), recognizes a small fixed set of common digit substitutions only between letters, and excludes exact terms listed in `names.rs` where ordinary technical, identity, health, or proper-name uses would make rejection too broad. It never scans arbitrary substrings.

The upstream English list was last changed in 2020, so it is a useful externally owned baseline rather than a complete or current moderation authority. Review policy exclusions and false-positive tests whenever updating it.

To update the snapshot:

1. Choose and record an upstream commit, then replace `en` and `LICENSE` from that commit.
2. Review the diff for newly ambiguous terms and update the exact exclusions only with a concrete false-positive case.
3. Run `cargo test -p jj-tandem-protocol names` and the hosted server tests.
4. Run `python3 scripts/check_docs.py` because this provenance file is checked as repository documentation.

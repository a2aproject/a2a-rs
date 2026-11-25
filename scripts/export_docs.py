#!/usr/bin/env python3
"""Export the docs as plain Markdown that another site can mount.

The A2A site (a2a-protocol.org) copies one directory of this repository and
renders it with its own MkDocs config. These pages pull code from outside
`docs/` with `--8<--` snippets, so they are expanded here, with the options
from `mkdocs.yml`, and the nav from `mkdocs.yml` is written to `.mount.yml`.

Usage: scripts/export_docs.py OUT_DIR   (run from the docs virtualenv)
"""

import argparse
import shutil
from pathlib import Path

import markdown
import yaml
from pymdownx.snippets import SnippetExtension

ROOT = Path(__file__).resolve().parent.parent
# The standalone site's own theme files; the mounting site brings its own.
SKIP = {'assets', 'stylesheets'}


class Loader(yaml.SafeLoader):
    """Reads mkdocs.yml, whose `!!python/name:` values are not needed here."""


Loader.add_multi_constructor('tag:yaml.org,2002:python/', lambda *_: None)


def snippet_options(config):
    for entry in config['markdown_extensions']:
        if isinstance(entry, dict) and 'pymdownx.snippets' in entry:
            return {**entry['pymdownx.snippets'], 'base_path': [str(ROOT)]}
    raise SystemExit('mkdocs.yml does not enable pymdownx.snippets')


def export(out):
    config = yaml.load((ROOT / 'mkdocs.yml').read_text(encoding='utf-8'), Loader)
    docs = ROOT / config.get('docs_dir', 'docs')
    snippets = markdown.Markdown(
        extensions=[SnippetExtension(**snippet_options(config))]
    ).preprocessors['snippet']

    shutil.rmtree(out, ignore_errors=True)
    for src in sorted(docs.rglob('*')):
        rel = src.relative_to(docs)
        if not src.is_file() or rel.parts[0] in SKIP:
            continue
        dest = out / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        if src.suffix == '.md':
            text = src.read_text(encoding='utf-8')
            expanded = '\n'.join(snippets.run(text.split('\n')))
            dest.write_text(expanded, encoding='utf-8')
        else:
            shutil.copy2(src, dest)

    nav = yaml.safe_dump({'nav': config['nav']}, sort_keys=False, allow_unicode=True)
    (out / '.mount.yml').write_text(nav, encoding='utf-8')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=(__doc__ or '').split('\n\n')[0])
    parser.add_argument('out', type=Path, help='directory to write, replaced if present')
    export(parser.parse_args().out.resolve())

#!/usr/bin/env bash
# Every repository path cited in prose or a comment must exist.
#
# This project points readers at files constantly — an ADR at an architecture section, a code
# comment at the script that tests it, an SDK at the error-code table. Those references rot
# silently: nothing fails to compile, nothing fails a test, and the reader simply finds nothing
# there and concludes the documentation is unreliable.
#
# The first run of this check found nine dead paths, including an error-code reference cited by
# three shipped SDKs under two different names, neither of which existed.
#
# A path that is deliberately not built yet should say so in the prose — mark it "planned" — and
# not be written as a live backticked path.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import os
import re
import sys

SKIP_DIRS = {'.git', 'target', 'node_modules', '.build', 'testdata', 'Pods'}
TEXT_EXTS = ('.rs', '.md', '.js', '.mjs', '.ts', '.cpp', '.h', '.hpp', '.sh', '.swift',
             '.java', '.kt', '.podspec', '.yml', '.yaml', '.toml')
ROOTS = ('crates', 'docs', 'sdk', 'scripts', 'benchmarks', 'testdata', 'examples')

# A backticked path that starts with one of the repository's top-level directories.
CITED = re.compile(r'`((?:' + '|'.join(ROOTS) + r')/[A-Za-z0-9_./{}-]+)`')
# A markdown link to something local.
LINKED = re.compile(r'\[[^\]]*\]\(([^)#\s]+)(?:#[^)]*)?\)')

problems = []


def check(path, text):
    for match in CITED.finditer(text):
        target = match.group(1)
        # Placeholders such as `crates/{name}/src` describe a shape, not a file.
        if '{' in target or '*' in target:
            continue
        if not os.path.exists(target):
            problems.append((path, target, 'cited path does not exist'))

    # Markdown links only. In Rust source, `[Storage](crate::storage::Storage)` is a rustdoc
    # intra-doc link to an item, not a file — and rustdoc checks those itself, which is why
    # `broken_intra_doc_links` is denied in the workspace lints.
    if not path.endswith('.md'):
        return

    directory = os.path.dirname(path)
    for match in LINKED.finditer(text):
        target = match.group(1)
        if target.startswith(('http://', 'https://', 'mailto:')):
            continue
        resolved = os.path.normpath(os.path.join(directory, target))
        if not os.path.exists(resolved):
            problems.append((path, target, 'link target does not exist'))


for root, dirs, files in os.walk('.'):
    dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
    for name in files:
        if not name.endswith(TEXT_EXTS):
            continue
        path = os.path.normpath(os.path.join(root, name))
        with open(path, encoding='utf-8', errors='replace') as handle:
            check(path, handle.read())

if problems:
    print(f'FAIL: {len(problems)} references point at nothing.\n')
    for path, target, why in sorted(problems):
        print(f'  {path}')
        print(f'    {target}  ({why})')
    print('\nEither create the file, fix the path, or — if it is not built yet — describe it as')
    print('planned in the prose rather than writing it as a live path.')
    sys.exit(1)

print('references: OK (every cited path and local link resolves)')
PY

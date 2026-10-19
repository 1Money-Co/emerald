"""Exercise real build metadata across archive, ref-change, and detached contexts.

Run from any directory with Python 3, Git, and cached Cargo dependencies.
Temporary repositories are discarded automatically; the source checkout is read only.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

source = Path(__file__).resolve().parents[3]
scratch = tempfile.TemporaryDirectory(prefix='emerald-build-context-')
root = Path(scratch.name)
archive = root / 'archive'
(archive / 'crates').mkdir(parents=True)
shutil.copytree(source / 'crates/build-info', archive / 'crates/build-info')
(archive / 'probe/src').mkdir(parents=True)
(archive / 'Cargo.toml').write_text('''[workspace]
resolver = "2"
members = ["crates/build-info", "probe"]
[workspace.package]
version = "0.0.1"
edition = "2021"
repository = "https://github.com/1Money-Co/emerald"
license = "Apache-2.0"
rust-version = "1.91.1"
publish = false
[workspace.lints]
''')
(archive / 'probe/Cargo.toml').write_text('''[package]
name = "emerald-build-context-probe"
version = "0.0.1"
edition = "2021"
[dependencies]
emerald-build-info = { path = "../crates/build-info" }
''')
(archive / 'probe/src/main.rs').write_text('''fn main() {
    use emerald_build_info::build_info;
    println!("{}|{}", build_info::COMMIT_HASH, build_info::BRANCH);
    let metadata = emerald_build_info::BuildMetadata::current();
    println!("metric:{}|{}", metadata.vcs_ref_head, metadata.vcs_ref_head_branch);
    println!("{}", build_info::CLAP_LONG_VERSION);
}
''')
shutil.copy2(source / 'Cargo.lock', archive / 'Cargo.lock')
env = os.environ.copy()
env.setdefault('CARGO_TARGET_DIR', str(source / 'target'))

def run(*args, cwd=root):
    return subprocess.check_output(args, cwd=cwd, env=env, text=True, stderr=subprocess.STDOUT).strip()

def commit(cwd, name):
    (cwd / name).write_text(name)
    run('git', 'add', name, cwd=cwd)
    run('git', '-c', 'user.name=Build Context Test', '-c', 'user.email=build-context@example.invalid', 'commit', '-qm', name, cwd=cwd)
    return run('git', 'rev-parse', 'HEAD', cwd=cwd)

def probe(workspace=archive):
    return run('cargo', 'run', '--offline', '--quiet', '--manifest-path', str(workspace / 'Cargo.toml'), '-p', 'emerald-build-context-probe', cwd=workspace)

run('git', 'init', '-q', '-b', 'unrelated-parent')
foreign_sha = commit(root, 'parent-marker')
run('git', 'tag', 'unrelated-parent-tag')
first = probe()
print('source archive in unrelated checkout:', first, flush=True)
assert first.splitlines()[0] == 'unknown|unknown', first
assert first.splitlines()[1] == 'metric:unknown|unknown'
assert foreign_sha not in first
assert 'unrelated-parent-tag' not in first, first
run('git', 'init', '-q', '-b', 'fixture-branch', cwd=archive)
first_sha = commit(archive, 'first-marker')
attached = probe()
print('attached:', attached, flush=True)
assert attached.splitlines()[0] == first_sha + '|fixture-branch'
second_sha = commit(archive, 'second-marker')
updated = probe()
print('ref updated without source edits:', updated, flush=True)
assert updated.splitlines()[0] == second_sha + '|fixture-branch'
run('git', 'checkout', '--detach', '-q', cwd=archive)
detached = probe()
print('detached:', detached, flush=True)
assert detached.splitlines()[0] in [second_sha + '|', second_sha + '|HEAD']
assert detached.splitlines()[1] == 'metric:' + second_sha + '|detached'

# Reuse the same Cargo target cache from a different source directory. The build
# script must inspect that directory, even if its executable came from this checkout.
other_archive = root / 'other-archive'
shutil.copytree(archive, other_archive, ignore=shutil.ignore_patterns('.git'))
other = probe(other_archive)
print('different source directory with cached build script:', other, flush=True)
assert other.splitlines()[0] == 'unknown|unknown', other
assert other.splitlines()[1] == 'metric:unknown|unknown'
assert foreign_sha not in other
assert 'unrelated-parent-tag' not in other, other
print(json.dumps({'result': 'PASS', 'scratch': str(root), 'scenarios': 5}), flush=True)

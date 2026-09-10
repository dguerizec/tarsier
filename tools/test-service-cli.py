#!/usr/bin/env python3
"""Exercise installation commands with an isolated home and fake systemctl.

Run after cargo build --bin tarsier. No real service or camera is modified.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / 'target/debug/tarsier'

with tempfile.TemporaryDirectory(prefix='tarsier-cli-') as temporary:
    base = Path(temporary)
    home = base / 'home'
    home.mkdir()
    working_directory = base / 'work %$ space'
    working_directory.mkdir()
    commands = base / 'bin'
    commands.mkdir()
    unit = home / '.config/systemd/user/tarsier.service'
    log = base / 'calls.jsonl'
    snapshot = base / 'state.json'
    current = subprocess.run(['systemctl', '--user', 'show', 'tarsier.service', '-p', 'MainPID', '--value'], capture_output=True, text=True).stdout.strip() or '0'
    state = {'LoadState': 'not-found', 'ActiveState': 'inactive', 'MainPID': current, 'FragmentPath': '', 'NRestarts': '0'}
    snapshot.write_text(json.dumps(state))
    fake = commands / 'systemctl'
    fake.write_text('''#!/usr/bin/python3
import json, os, pathlib, sys
args = sys.argv[1:]
with open(os.environ['TEST_CALLS'], 'a') as f: f.write(json.dumps(args) + '\\n')
state = json.loads(pathlib.Path(os.environ['TEST_STATE']).read_text())
if 'show' in args:
    print('\\n'.join(k + '=' + v for k, v in state.items()))
''')
    fake.chmod(0o755)
    env = dict(os.environ, HOME=str(home), XDG_CONFIG_HOME=str(home / '.config'), XDG_STATE_HOME=str(home / '.local/state'), PATH=f'{commands}:' + os.environ['PATH'], TEST_CALLS=str(log), TEST_STATE=str(snapshot))
    for key in ('TARSIER_USER_SETTINGS_PATH', 'TARSIER_AUTH_PATH', 'TARSIER_API_TOKEN'):
        env.pop(key, None)
    config = base / 'safe % config.toml'
    config.write_text('[video]\nsource = "test"\nloopback_enabled = false\n[perception]\nenabled = false\n[camera]\nadapter = "mock"\n')

    def run(*args, success=True):
        result = subprocess.run([str(BINARY), *args], env=env, cwd=working_directory, text=True, capture_output=True, timeout=60)
        assert (result.returncode == 0) == success, result.stdout + result.stderr
        return result

    def calls():
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    run('install', '--config', str(config))
    text = unit.read_text()
    assert 'safe %% config.toml' in text
    assert 'TARSIER_USER_SETTINGS_PATH=' in text
    assert not any('restart' in c or 'stop' in c for c in calls())
    run('install', '--config', str(config))
    assert unit.read_text() == text
    verification = subprocess.run(['systemd-analyze', '--user', 'verify', str(unit)], env=env, capture_output=True, text=True)
    assert verification.returncode == 0, verification.stderr
    other = base / 'other.toml'
    other.write_text(config.read_text())
    run('install', '--config', str(other), success=False)
    assert unit.read_text() == text
    run('install', '--config', str(other), '--replace')
    assert unit.read_text() != text

    state.update(LoadState='loaded', ActiveState='active', FragmentPath=str(unit))
    snapshot.write_text(json.dumps(state))
    run('uninstall', success=False)
    assert unit.exists()
    state['FragmentPath'] = '/run/user/1000/systemd/transient/tarsier.service'
    snapshot.write_text(json.dumps(state))
    run('uninstall', '--now', success=False)
    assert unit.exists()
    state['FragmentPath'] = str(unit)
    snapshot.write_text(json.dumps(state))
    # Explicit --now starts only through the same service name.
    run('install', '--config', str(other), '--now')
    assert ['--user', 'restart', 'tarsier.service'] in calls()
    media = home / 'Pictures' / 'preserved.jpg'
    media.parent.mkdir()
    media.write_bytes(b'preserved')
    run('uninstall', '--now')
    assert not unit.exists() and media.read_bytes() == b'preserved'
    run('uninstall')

    unit.parent.mkdir(parents=True, exist_ok=True)
    unit.write_text('[Service]\nExecStart=/bin/true\n')
    run('uninstall', '--now', success=False)
    run('install', '--config', str(config), success=False)
    assert unit.read_text().startswith('[Service]')
    run('install', '--config', str(config), '--replace')
    unit.write_text(unit.read_text() + '# manual edit\n')
    run('uninstall', '--now', success=False)

    # Transient services require an explicit immediate migration.
    unit.unlink()
    state.update(FragmentPath='/run/user/1000/systemd/transient/tarsier.service')
    snapshot.write_text(json.dumps(state))
    run('install', '--config', str(config), success=False)
    run('install', '--config', str(config), '--replace', success=False)
    run('install', '--config', str(config), '--replace', '--now')
    recent = calls()
    assert ['--user', 'stop', 'tarsier.service'] in recent

    # Doctor only reads settings; it must not create the normal state store.
    state.update(ActiveState='inactive', FragmentPath=str(unit))
    snapshot.write_text(json.dumps(state))
    run('doctor', '--config', str(config))
    assert not (home / '.local/state/tarsier/user-settings.json').exists()
    assert media.read_bytes() == b'preserved'
    print('Service CLI integration checks passed.')

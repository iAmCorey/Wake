# Read-only remote inventory. stdout contains metadata, never memory bodies or settings secrets.
import hashlib, json, os, re, stat, sys
from pathlib import Path

home = Path.home()
primary = Path('.codebuddy/projects')
fallback = Path('.codebuddy/memories')
ide = Path('Library/Application Support/CodeBuddyExtension/Data/Public/.memories' if sys.platform == 'darwin' else '.local/share/CodeBuddyExtension/Data/Public/.memories')
dirs, files = set(), {}
project_paths = {}
limit = 4 * 1024 * 1024

def inspect(rel):
    path = home
    for part in rel.parts:
        if part in ('..', '') or '\n' in part or '\r' in part:
            raise ValueError('Unsafe memory path')
        path /= part
        try:
            info = path.lstat()
        except FileNotFoundError:
            return None
        if stat.S_ISLNK(info.st_mode):
            raise ValueError('Memory symlink is unsupported')
    return info

def read(rel):
    info = inspect(rel)
    if info is None:
        return None
    if not stat.S_ISREG(info.st_mode) or info.st_size > limit:
        raise ValueError('Invalid memory file')
    with (home / rel).open('rb') as f:
        body = f.read(limit + 1)
    if len(body) > limit:
        raise ValueError('Memory file too large')
    body.decode('utf-8')
    return body

def walk(rel, recursive, mdc=False):
    info = inspect(rel)
    if info is None:
        return
    if not stat.S_ISDIR(info.st_mode):
        raise ValueError('Invalid memory directory')
    dirs.add(str(rel))
    for p in sorted((home / rel).iterdir()):
        if p.name.startswith('.'):
            continue
        child = rel / p.name
        info = inspect(child)
        if info is None:
            raise ValueError('Memory changed during enumeration')
        if stat.S_ISDIR(info.st_mode):
            if recursive:
                walk(child, True, mdc)
        elif p.suffix == '.md' or (mdc and p.suffix == '.mdc'):
            files[str(child)] = hashlib.sha256(read(child)).hexdigest()

def project_id(value):
    base = re.sub('-+', '-', re.sub(r'[/\\:]', '-', value)).strip('-')
    data = base.encode('utf-8')
    if len(data) <= 255:
        return base
    h = 5381
    for b in data:
        h = ((h * 33) ^ b) & 0xffffffff
    digits = ''
    while h:
        digits = '0123456789abcdefghijklmnopqrstuvwxyz'[h % 36] + digits
        h //= 36
    return data[:180].decode('utf-8', 'ignore') + '-' + (digits or '0')

def attribute(identifier):
    root = primary / identifier
    if inspect(root) is None:
        return
    candidates = set()
    for p in sorted((home / root).glob('*.jsonl')):
        info = inspect(root / p.name)
        if info is None or not stat.S_ISREG(info.st_mode):
            raise ValueError('Invalid memory attribution source')
        with p.open('rb') as f:
            data = f.read(1024 * 1024)
        for line in data.splitlines()[:128]:
            try:
                row = json.loads(line)
            except (ValueError, UnicodeError):
                continue
            cwd = row.get('cwd') if isinstance(row, dict) else None
            if not isinstance(cwd, str) or not cwd:
                continue
            canonical = os.path.realpath(cwd) if os.path.exists(cwd) else cwd
            if project_id(canonical) == identifier:
                candidates.add(canonical)
            elif project_id(cwd) == identifier:
                candidates.add(cwd)
            break
    if candidates:
        project_paths[identifier] = sorted(candidates)

def projects(root, old=False):
    if inspect(root) is None:
        return
    dirs.add(str(root))
    for p in sorted((home / root).iterdir()):
        if p.name.startswith('.') or (old and p.name == 'global'):
            continue
        rel = root / p.name
        info = inspect(rel)
        if info and stat.S_ISDIR(info.st_mode):
            memory = rel if old else rel / 'memory'
            if inspect(memory) is not None:
                walk(memory, True)
                attribute(p.name)

projects(primary)
projects(fallback, True)
walk(fallback / 'global', True)
walk(ide, False, True)

def config(rel):
    body = read(rel)
    value = json.loads(body) if body is not None else {}
    if not isinstance(value, dict):
        raise ValueError('Invalid memory settings')
    return value

def flag(value, key, default=True):
    result = value.get(key, default)
    if not isinstance(result, bool):
        raise ValueError('Invalid memory switch')
    return result

cli = flag(config(Path('.codebuddy/settings.json')).get('memory', {}), 'autoMemoryEnabled')
settings = dict(cli=cli, project=flag(config(Path('.codebuddy/ide-memory-settings.json')), 'projectEnabled', cli), global_=flag(config(ide / '.memory-global-config.json'), 'enabled'))
settings['global'] = settings.pop('global_')
print('WAKE_MEMORY:' + json.dumps(dict(dirs=sorted(dirs), files=files, settings=settings, projects=project_paths), sort_keys=True))

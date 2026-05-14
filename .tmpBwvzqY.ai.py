import os, sys
os.environ.setdefault('KODA_AGENT_HOME', "/Users/vanzheng/.koda-agent")
os.environ.setdefault('KODA_AGENT_ROOT', "/Users/vanzheng/projects/rust/koda-agent")
os.environ.setdefault('KODA_WORKSPACE', "/Users/vanzheng/projects/rust/koda-agent")
os.environ.setdefault('KODA_MEMORY_DIR', "/Users/vanzheng/.koda-agent/memory")
for _koda_p in ["/Users/vanzheng/.koda-agent/memory", os.path.join("/Users/vanzheng/projects/rust/koda-agent", 'memory')]:
    if _koda_p and _koda_p not in sys.path: sys.path.insert(0, _koda_p)
import sys, os, json, re, time, subprocess
sys.path.append(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'memory'))
_r = subprocess.run
def _d(b):
    if not b: return ''
    if isinstance(b, str): return b
    try: return b.decode()
    except: return b.decode('gbk', 'replace')
def _run(*a, **k):
    t = k.pop('text', 0) | k.pop('universal_newlines', 0)
    enc = k.pop('encoding', None)
    k.pop('errors', None)
    if enc: t = 1
    if t and isinstance(k.get('input'), str):
        k['input'] = k['input'].encode()
    r = _r(*a, **k)
    if t:
        if r.stdout is not None: r.stdout = _d(r.stdout)
        if r.stderr is not None: r.stderr = _d(r.stderr)
    return r
subprocess.run = _run
_Pi = subprocess.Popen.__init__
def _pinit(self, *a, **k):
    if os.name == 'nt': k['creationflags'] = (k.get('creationflags') or 0) | 0x08000000
    _Pi(self, *a, **k)
subprocess.Popen.__init__ = _pinit
sys.excepthook = lambda t, v, tb: (sys.__excepthook__(t, v, tb), print(f"\n[Agent Hint]: NO GUESSING! You MUST probe first. If missing common package, pip.")) if issubclass(t, (ImportError, AttributeError)) else sys.__excepthook__(t, v, tb)

import subprocess, os
os.chdir("/Users/vanzheng/projects/rust/koda-agent")

# Stage and commit the clippy fixes
subprocess.run(["git", "add", "-A"], check=True)
r = subprocess.run(["git", "commit", "-m", "style: fix clippy collapsible_if and unnecessary_unwrap warnings"], 
                    capture_output=True, text=True)
print(f"=== commit: rc={r.returncode} ===")
print(r.stdout)
if r.stderr:
    print(r.stderr[:500])

# Push
r2 = subprocess.run(["git", "push", "origin", "main"], capture_output=True, text=True)
print(f"=== push: rc={r2.returncode} ===")
print(r2.stdout)
if r2.stderr:
    print(r2.stderr[:500])

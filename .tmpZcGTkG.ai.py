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

# Commit formatting fix
r1 = subprocess.run(["git", "add", "-A"], capture_output=True, text=True)
print(f"=== git add: rc={r1.returncode} ===")

r2 = subprocess.run(["git", "commit", "-m", "style: apply cargo fmt to pass CI format check"], capture_output=True, text=True)
print(f"=== commit: rc={r2.returncode} ===")
print(r2.stdout)

# Push
r3 = subprocess.run(["git", "push", "origin", "main"], capture_output=True, text=True, timeout=30)
print(f"=== push: rc={r3.returncode} ===")
if r3.returncode != 0:
    print(f"STDERR: {r3.stderr}")

# Verify
r4 = subprocess.run(["git", "log", "--oneline", "-3"], capture_output=True, text=True)
print(f"\n=== main log ===\n{r4.stdout}")

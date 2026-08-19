#!/usr/bin/env python3
"""Submit one trellis-2 mesh job and print every progress-label change with
timestamps: mesh_probe.py <base_url> <input.png> [out.glb] [--seed N]"""
import base64, json, sys, time, urllib.request, urllib.error

def call(url, body=None, timeout=60):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method="POST" if body is not None else "GET",
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()

base, png = sys.argv[1], sys.argv[2]
out = sys.argv[3] if len(sys.argv) > 3 and not sys.argv[3].startswith("--") else None
seed = None
if "--seed" in sys.argv:
    seed = int(sys.argv[sys.argv.index("--seed") + 1])
req = {"model": "trellis-2", "input_b64": base64.b64encode(open(png, "rb").read()).decode()}
if seed is not None:
    req["seed"] = seed
t0 = time.time()
code, body = call(f"{base}/generate", req)
if code != 200:
    print("SUBMIT FAIL", code, body[:300]); sys.exit(1)
job = json.loads(body)["job_id"]
print(f"job {job}", flush=True)
last = None
while True:
    code, body = call(f"{base}/job/{job}")
    j = json.loads(body)
    key = (j.get("state"), j.get("stage") or j.get("label") or j.get("detail"), round(float(j.get("progress") or 0), 3))
    if key != last:
        print(f"{time.time()-t0:7.1f}s  {key}", flush=True)
        last = key
    if j.get("state") in ("done", "error", "cancelled"):
        print(json.dumps({k: v for k, v in j.items() if k != "artifacts"})[:600])
        break
    time.sleep(1.0)
if out and j.get("state") == "done":
    arts = j.get("artifacts") or []
    for a in arts:
        aid = a.get("id") or a.get("artifact_id")
        ct = a.get("content_type", "")
        if "gltf" in ct or "glb" in ct:
            code, blob = call(f"{base}/artifact/{aid}", timeout=300)
            open(out, "wb").write(blob); print(f"saved {out} ({len(blob)} bytes)")
            break
print(f"total {time.time()-t0:.1f}s")

"""Click the studio and screenshot the result, for checking an interaction.

Not part of the application. It exists because a screenshot proves what was drawn
and nothing about what happens when a person clicks it, and the interactions worth
checking -- isolating a country by clicking its name, picking a bus out of eight
thousand -- cannot be reached any other way from a headless browser.

**It drives Chrome's own input pipeline through the DevTools Protocol rather than
dispatching DOM events.** A synthetic PointerEvent on the canvas does not reach
eframe: several rounds of them, with `buttons` set correctly and both the pointer
and mouse families dispatched, changed not one pixel -- including a click on the
Solve button, which plainly works when a person presses it. CDP input is
indistinguishable from a person, and it worked first time.

Coordinates are **pixels measured off a screenshot this script produced**, not
fractions of the window: headless Chrome's viewport is shorter than the window it
is given, so a fraction of the window and a fraction of the picture are two
different places.

usage: click-probe.py <url> <out.png> [<x> <y>]...
"""
import base64, json, os, subprocess, sys, time, urllib.request
import websocket

URL, OUT = sys.argv[1], sys.argv[2]
# Pixel coordinates, read straight off a screenshot this script produced. Headless
# Chrome's viewport is shorter than the window it was given, so a fraction of the
# window and a fraction of the picture are two different places.
CLICKS = [(float(a), float(b)) for a, b in zip(sys.argv[3::2], sys.argv[4::2])]
W, H, PORT = 1440, 940, 9333
# Device pixel ratio to emulate. A Retina display is 2, and rendering at 1 there is
# what "the text looks low-resolution" means -- so it has to be checkable.
DPR = float(os.environ.get("PROBE_DPR", "1"))

chrome = subprocess.Popen([
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "--headless=new", "--disable-gpu", "--enable-unsafe-swiftshader",
    "--use-gl=angle", "--use-angle=swiftshader",
    f"--remote-debugging-port={PORT}", f"--window-size={W},{H}",
    # Chrome refuses a WebSocket from an origin it was not told to expect.
    "--remote-allow-origins=*",
    "--no-first-run", f"--user-data-dir=/tmp/cdp_profile_{PORT}", URL,
], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

def targets():
    for _ in range(60):
        try:
            d = json.load(urllib.request.urlopen(f"http://127.0.0.1:{PORT}/json"))
            page = [t for t in d if t["type"] == "page" and "devtoolsFrontendUrl" in t]
            if page:
                return page[0]
        except Exception:
            pass
        time.sleep(0.5)
    raise SystemExit("no debuggable page")

try:
    ws = websocket.create_connection(targets()["webSocketDebuggerUrl"], timeout=60)
    n = [0]
    def send(method, **params):
        n[0] += 1
        ws.send(json.dumps({"id": n[0], "method": method, "params": params}))
        while True:
            msg = json.loads(ws.recv())
            if msg.get("id") == n[0]:
                return msg.get("result", {})

    if DPR != 1.0:
        send("Emulation.setDeviceMetricsOverride", width=W, height=H,
             deviceScaleFactor=DPR, mobile=False)

    # The wasm has to instantiate and lay out before a click means anything.
    time.sleep(14)
    for x, y in CLICKS:
        send("Input.dispatchMouseEvent", type="mouseMoved", x=x, y=y, buttons=0)
        time.sleep(0.4)
        send("Input.dispatchMouseEvent", type="mousePressed", x=x, y=y,
             button="left", buttons=1, clickCount=1)
        time.sleep(0.25)
        send("Input.dispatchMouseEvent", type="mouseReleased", x=x, y=y,
             button="left", buttons=0, clickCount=1)
        time.sleep(1.6)
    time.sleep(2)
    shot = send("Page.captureScreenshot", format="png")
    open(OUT, "wb").write(base64.b64decode(shot["data"]))
    print("wrote", OUT)
finally:
    chrome.terminate()

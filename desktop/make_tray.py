"""The menu bar masks of Pulse: renders assets/tray-sprite.html with Chrome
and cuts it into one alpha mask per state symbol and digit (src-tauri/icons/tray).

Usage: python3 make_tray.py
"""

import subprocess
from pathlib import Path

from PIL import Image, ImageOps

HERE = Path(__file__).parent
OUT = HERE / "src-tauri/icons/tray"
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
SHEET = OUT / "sheet.png"

OUT.mkdir(parents=True, exist_ok=True)
subprocess.run([CHROME, "--headless=new", "--disable-gpu", "--hide-scrollbars", "--force-device-scale-factor=1",
                "--window-size=400,80", f"--screenshot={SHEET}", (HERE / "assets/tray-sprite.html").as_uri()],
               check=True, capture_output=True)
sheet = ImageOps.invert(Image.open(SHEET).convert("L"))
for i, name in enumerate(["blocked", "waiting", "review", "commands", "done", "running"]):
    cell = sheet.crop((i * 40 + 3, 3, i * 40 + 33, 33))
    Image.merge("LA", (Image.new("L", cell.size, 255), cell)).save(OUT / f"{name}.png")
for d in range(10):
    cell = sheet.crop((d * 40, 40, d * 40 + 36, 76))
    box = cell.getbbox()
    cell = cell.crop((box[0], 0, box[2], 36))
    Image.merge("LA", (Image.new("L", cell.size, 255), cell)).save(OUT / f"digit{d}.png")
SHEET.unlink()
print(sorted(p.name for p in OUT.iterdir()))

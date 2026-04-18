---
name: preview-site
description: "Take a screenshot of the site and visually inspect it. Use when making changes to site/ and wanting to verify the result."
user-invocable: true
---

# Preview Site

Take a headless browser screenshot of `site/index.html` to visually inspect layout, colors, spacing, and alignment.

## Steps

1. Run headless Chromium to capture a full-page screenshot:

```bash
chromium --headless \
  --screenshot=/tmp/tau-site-preview.png \
  --window-size=1280,4000 \
  --disable-gpu --no-sandbox \
  "file://$(pwd)/site/index.html"
```

2. Read the screenshot image at `/tmp/tau-site-preview.png` using the Read tool -- it supports images natively.

3. Inspect the rendering and report any issues with layout, alignment, spacing, colors, or font consistency.

4. Clean up: `rm /tmp/tau-site-preview.png`

## Notes

- The `--window-size=1280,4000` height should be tall enough to capture the full page without scrolling. Increase if the page grows.
- Chromium will emit dbus/vaapi warnings on headless servers -- these are harmless and can be ignored.
- For mobile layout testing, use `--window-size=375,2000`.

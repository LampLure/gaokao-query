#!/usr/bin/env python3
"""
OCR helper: find captcha character positions via connected components,
recognize each via tesseract, then reorder to match expected click order.
"""

import sys
import os
from PIL import Image

os.environ["TESSDATA_PREFIX"] = "/tmp"


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def find_char_boxes(image_path):
    """Find character bounding boxes via connected components on binarized image."""
    img = Image.open(image_path)
    if img.mode == 'RGBA':
        img = img.convert('RGB')
    gray = img.convert('L')
    iw, ih = img.size
    log(f"Image size: {iw}x{ih}")

    # Binarize: text is dark on light background
    pixels = list(gray.getdata())
    # Find a good threshold - look for the dark pixels
    sorted_p = sorted(pixels)
    threshold = sorted_p[len(sorted_p) * 20 // 100]  # top 20% dark
    threshold = min(threshold, 160)
    log(f"Threshold: {threshold}")

    bw = gray.point(lambda x: 0 if x < threshold else 255)
    bw.save("/tmp/gaokao_bw.png")
    log("Saved binarized image to /tmp/gaokao_bw.png")

    # Find connected components using scan-line flood fill
    w, h = bw.size
    visited = [[False] * w for _ in range(h)]
    components = []

    for y in range(h):
        for x in range(w):
            if visited[y][x]:
                continue
            if bw.getpixel((x, y)) != 0:  # not dark pixel
                visited[y][x] = True
                continue

            # BFS flood fill for this component
            stack = [(x, y)]
            visited[y][x] = True
            min_x, max_x = x, x
            min_y, max_y = y, y
            count = 0

            while stack:
                cx, cy = stack.pop()
                count += 1
                min_x = min(min_x, cx)
                max_x = max(max_x, cx)
                min_y = min(min_y, cy)
                max_y = max(max_y, cy)
                # Check 4-connected neighbors
                for nx, ny in [(cx - 1, cy), (cx + 1, cy), (cx, cy - 1), (cx, cy + 1)]:
                    if 0 <= nx < w and 0 <= ny < h and not visited[ny][nx]:
                        if bw.getpixel((nx, ny)) == 0:
                            visited[ny][nx] = True
                            stack.append((nx, ny))

            cw = max_x - min_x + 1
            ch = max_y - min_y + 1
            # Filter: character-like components
            if count >= 15 and cw >= 8 and ch >= 8 and cw <= iw * 0.4 and ch <= ih * 0.5:
                components.append((min_x, min_y, max_x, max_y, count))

    log(f"Found {len(components)} character-like components:")
    for x1, y1, x2, y2, sz in components:
        log(f"  ({x1},{y1})-({x2},{y2})  size={x2-x1}x{y2-y1}  pixels={sz}")

    if len(components) < 3:
        log(f"WARNING: Only {len(components)} components found, too few")
        return []

    # Sort by x position, take 3
    components.sort(key=lambda c: c[0])
    # If more than 3, try to pick the 3 most likely (by pixel count, position)
    if len(components) > 3:
        # Prefer components in the middle band (y = 25%-75% of height)
        mid_y_low = ih * 0.25
        mid_y_high = ih * 0.75
        scored = []
        for c in components:
            cy = (c[1] + c[3]) / 2
            y_score = 1.0 - abs(cy - ih / 2) / (ih / 2)  # closer to center = better
            size_score = min(1.0, c[4] / 200)  # bigger = better up to a point
            scored.append((y_score * 0.6 + size_score * 0.4, c))
        scored.sort(key=lambda s: -s[0])
        components = [c for _, c in scored[:3]]
        components.sort(key=lambda c: c[0])

    log(f"\nSelected 3 components (left-to-right):")
    for x1, y1, x2, y2, sz in components:
        log(f"  ({x1},{y1})-({x2},{y2})")

    return [(x1, y1, x2, y2) for x1, y1, x2, y2, _ in components]


def recognize_char(image_path, box):
    """Crop and recognize a single character using tesseract."""
    import tesserocr
    x1, y1, x2, y2 = box
    try:
        img = Image.open(image_path)
        crop = img.crop((x1, y1, x2, y2))
        crop_gray = crop.convert('L')
        # Enlarge for better recognition
        crop_big = crop_gray.resize((crop_gray.width * 3, crop_gray.height * 3), Image.LANCZOS)

        api = tesserocr.PyTessBaseAPI(path="/tmp", lang="chi_sim")
        try:
            api.SetImage(crop_big)
            api.SetVariable("tessedit_pageseg_mode", "10")  # single char
            api.Recognize()
            text = api.GetUTF8Text().strip()
            if text:
                # Take first char only
                return text[0]
        finally:
            api.End()
    except Exception as e:
        log(f"  Recognize error: {e}")
    return ""


def solve(image_path, expected_chars):
    log("--- OCR Debug ---")
    log(f"Expected click order: {' → '.join(expected_chars)}")

    # Step 1: find character positions via connected components
    boxes = find_char_boxes(image_path)

    if len(boxes) < 3:
        log("ERROR: Could not find 3 character positions in image")
        sys.exit(1)

    # Step 2: recognize each character
    log(f"\nCharacter recognition:")
    recognized = []
    for i, box in enumerate(boxes):
        ch = recognize_char(image_path, box)
        recognized.append(ch)
        log(f"  Box[{i}] at ({box[0]},{box[1]})-({box[2]},{box[3]}): '{ch}'")

    # Step 3: match recognized chars to expected order
    assigned = {}   # expected_char -> box_index
    used = set()
    unmatched = []

    for ch in expected_chars:
        found = False
        for idx, rch in enumerate(recognized):
            if idx in used:
                continue
            if rch and (ch == rch or ch in rch or rch in ch):
                assigned[ch] = idx
                used.add(idx)
                found = True
                break
        if not found:
            unmatched.append(ch)

    log(f"\nMatching results:")
    for ch, idx in assigned.items():
        log(f"  '{ch}' → box[{idx}] (recognized as '{recognized[idx]}')")
    if unmatched:
        log(f"  Unmatched: {unmatched}")

    # Assign remaining boxes to unmatched chars
    remaining = [i for i in range(len(boxes)) if i not in used]
    for ch, idx in zip(unmatched, remaining):
        assigned[ch] = idx
        log(f"  '{ch}' → box[{idx}] (by position, recognized as '{recognized[idx]}')")

    # If still missing, fall back to position
    if len(assigned) < 3:
        log("WARNING: Not all assigned, using left-to-right fallback")
        for i, ch in enumerate(expected_chars):
            assigned[ch] = i

    # Build final result
    result = []
    log(f"\nFinal click order:")
    for ch in expected_chars:
        idx = assigned[ch]
        box = boxes[idx]
        result.append(box)
        log(f"  '{ch}' at ({box[0]},{box[1]})-({box[2]},{box[3]})")

    return result


def main():
    if len(sys.argv) < 3:
        log("Usage: ocr_helper.py <image_path> <char1 char2 char3>")
        sys.exit(1)

    image_path = sys.argv[1]
    expected_chars = sys.argv[2].strip().split()

    if not os.path.exists(image_path):
        log(f"ERROR: Image not found: {image_path}")
        sys.exit(1)

    matched = solve(image_path, expected_chars)

    if len(matched) < 3:
        log(f"ERROR: Only matched {len(matched)} positions")
        sys.exit(1)

    try:
        iw, ih = Image.open(image_path).size
    except Exception:
        iw, ih = 300, 150

    log(f"\nFinal coordinates (fraction of {iw}x{ih}):")
    for i, (x1, y1, x2, y2) in enumerate(matched):
        cx = (x1 + x2) / 2.0 / iw
        cy = (y1 + y2) / 2.0 / ih
        log(f"  Click {i+1}: ({cx:.4f}, {cy:.4f})")
        print(f"{cx:.4f},{cy:.4f}")

    log("--- OCR Done ---")


if __name__ == "__main__":
    main()

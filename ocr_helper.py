#!/usr/bin/env python3
"""
OCR helper: given a captcha image and 3 expected characters in order,
use tesseract to identify each character in the image,
then reorder them to match the expected click order.
"""

import sys
import os
from PIL import Image

os.environ["TESSDATA_PREFIX"] = "/tmp"


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def solve(image_path, expected_chars):
    import tesserocr

    log("--- OCR Debug ---")
    log(f"Expected click order: {' → '.join(expected_chars)}")

    api = tesserocr.PyTessBaseAPI(path="/tmp", lang="chi_sim")
    try:
        api.SetImageFile(image_path)
        api.SetVariable("tessedit_pageseg_mode", "6")
        api.Recognize()

        # Get symbol-level recognition results
        boxes = []
        ri = api.GetIterator()
        if ri:
            level = tesserocr.RIL.SYMBOL
            while True:
                text = ri.GetUTF8Text(level).strip()
                bbox = ri.BoundingBox(level)
                if bbox and text:
                    x1, y1, x2, y2 = bbox
                    boxes.append((text, x1, y1, x2, y2))
                if not ri.Next(level):
                    break

        log(f"Tesseract recognized {len(boxes)} symbols:")
        for text, x1, y1, x2, y2 in boxes:
            log(f"  '{text}' at ({x1},{y1})-({x2},{y2})  size={x2-x1}x{y2-y1}")

        if len(boxes) < 3:
            log(f"WARNING: Only {len(boxes)} symbols recognized, trying component boxes")
            comps = api.GetComponentImages(tesserocr.RIL.SYMBOL, True)
            boxes = []
            for _, b, _, _ in comps:
                x1, y1 = b["x"], b["y"]
                x2, y2 = x1 + b["w"], y1 + b["h"]
                # Try to recognize each component
                crop = api.GetImage().crop((x1, y1, x2, y2))
                crop_api = tesserocr.PyTessBaseAPI(path="/tmp", lang="chi_sim")
                try:
                    crop_api.SetImage(crop)
                    crop_api.SetVariable("tessedit_pageseg_mode", "10")
                    text = crop_api.GetUTF8Text().strip()
                finally:
                    crop_api.End()
                boxes.append((text if text else "?", x1, y1, x2, y2))
            log(f"Component boxes: {len(boxes)}")
            for text, x1, y1, x2, y2 in boxes:
                log(f"  '{text}' at ({x1},{y1})-({x2},{y2})")

        # If we have 3+ recognized symbols, use recognition-based matching
        # Build a map from recognized character -> (box, position)
        char_to_box = {}
        for text, x1, y1, x2, y2 in boxes:
            for ch in text:
                if ch.strip():
                    char_to_box[ch] = (x1, y1, x2, y2)

        log(f"\nCharacter map: { {k: f'({v[0]},{v[1]})-({v[2]},{v[3]})' for k, v in char_to_box.items()} }")

        # Match expected chars to positions
        result_boxes = []
        missing = []
        for ch in expected_chars:
            if ch in char_to_box:
                result_boxes.append(char_to_box[ch])
            else:
                missing.append(ch)
                result_boxes.append(None)

        if missing:
            log(f"WARNING: Characters {missing} not found in recognition results!")
            log("Falling back to position-based matching (left-to-right)")

        # Fill in missing slots with remaining boxes sorted by x
        missing_indices = [i for i, b in enumerate(result_boxes) if b is None]
        if missing_indices:
            used = {id(b) for b in result_boxes if b}
            remaining = sorted(
                [(x1, y1, x2, y2) for _, x1, y1, x2, y2 in boxes
                 if id((x1, y1, x2, y2)) not in used],
                key=lambda b: b[0]
            )
            for idx, box in zip(missing_indices, remaining):
                result_boxes[idx] = box

        # Verify all slots filled
        result_boxes = [b for b in result_boxes if b is not None]

        log(f"\nFinal order:")
        for i, (ch, (x1, y1, x2, y2)) in enumerate(zip(expected_chars, result_boxes)):
            log(f"  Click {i+1}: '{ch}' at ({x1},{y1})-({x2},{y2})")

        return result_boxes

    finally:
        api.End()


def main():
    if len(sys.argv) < 3:
        print("Usage: ocr_helper.py <image_path> <char1 char2 char3>", file=sys.stderr)
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

    img = Image.open(image_path)
    iw, ih = img.size
    log(f"Image dimensions: {iw}x{ih}")

    log(f"\nFinal click coordinates (fraction of {iw}x{ih}):")
    for i, (x1, y1, x2, y2) in enumerate(matched):
        cx = (x1 + x2) / 2.0 / iw
        cy = (y1 + y2) / 2.0 / ih
        log(f"  Click {i+1}: ({cx:.4f}, {cy:.4f})")
        print(f"{cx:.4f},{cy:.4f}")

    log("--- OCR Done ---")


if __name__ == "__main__":
    main()

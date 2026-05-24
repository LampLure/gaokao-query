#!/usr/bin/env python3
"""
OCR helper: given a captcha image and 3 expected characters in order,
use tesseract to identify each character in the image,
then reorder them to match the expected click order.

Strategy:
1. Use tesseract OCR to recognize text at each character position
2. For each expected char, check if it was correctly recognized
3. Assign recognized chars to their boxes, remaining boxes to unmatched chars
4. If no chars are recognized, fall back to left-to-right position matching
"""

import sys
import os
from PIL import Image

os.environ["TESSDATA_PREFIX"] = "/tmp"

def log(msg):
    print(msg, file=sys.stderr, flush=True)

def preprocess_image(image_path):
    """Enhance image contrast and binarize for better OCR."""
    try:
        img = Image.open(image_path)
        if img.mode == 'RGBA':
            img = img.convert('RGB')
        gray = img.convert('L')
        from PIL import ImageOps, ImageFilter, ImageEnhance
        # Auto contrast
        gray = ImageOps.autocontrast(gray, cutoff=3)
        # Sharpen
        gray = gray.filter(ImageFilter.SHARPEN)
        # Binarize: apply threshold
        gray = gray.point(lambda x: 0 if x < 180 else 255)
        gray.save(image_path)
        return True
    except Exception as e:
        log(f"Preprocess error: {e}")
        return False


def try_ocr(image_path):
    """Try OCR with multiple PSM modes, return first successful result."""
    import tesserocr

    psm_modes = [6, 3, 11, 12, 13, 4]
    for psm in psm_modes:
        try:
            api = tesserocr.PyTessBaseAPI(path="/tmp", lang="chi_sim")
            try:
                api.SetImageFile(image_path)
                api.SetVariable("tessedit_pageseg_mode", str(psm))
                api.Recognize()

                boxes = []
                ri = api.GetIterator()
                if ri:
                    level = tesserocr.RIL.SYMBOL
                    while True:
                        try:
                            text = ri.GetUTF8Text(level).strip()
                        except RuntimeError:
                            text = ""
                        bbox = ri.BoundingBox(level)
                        if bbox and text and text.isprintable():
                            x1, y1, x2, y2 = bbox
                            w, h = x2 - x1, y2 - y1
                            if w >= 5 and h >= 5:
                                boxes.append((text, x1, y1, x2, y2))
                        if not ri.Next(level):
                            break
                if len(boxes) >= 3:
                    return boxes, psm
            finally:
                api.End()
        except Exception as e:
            log(f"PSM {psm} error: {e}")
    return [], None


def solve(image_path, expected_chars):
    import tesserocr

    log("--- OCR Debug ---")
    log(f"Expected click order: {' → '.join(expected_chars)}")
    log(f"Image size: {Image.open(image_path).size}")

    preprocess_image(image_path)

    boxes, psm = try_ocr(image_path)
    log(f"PSM mode: {psm}")
    log(f"Tesseract recognized {len(boxes)} symbols:")
    for text, x1, y1, x2, y2 in boxes:
        log(f"  '{text}' at ({x1},{y1})-({x2},{y2})  size={x2-x1}x{y2-y1}")

    if len(boxes) < 3:
        log(f"WARNING: Only {len(boxes)} symbols from OCR, trying CC fallback")
        api = tesserocr.PyTessBaseAPI(path="/tmp", lang="chi_sim")
        try:
            api.SetImageFile(image_path)
            comps = api.GetComponentImages(tesserocr.RIL.SYMBOL, True)
            boxes = []
            for _, b, _, _ in comps:
                x1, y1 = b["x"], b["y"]
                w, h = b["w"], b["h"]
                if w < 5 or h < 5:
                    continue
                boxes.append(("", x1, y1, x1 + w, y1 + h))
            log(f"CC boxes: {len(boxes)}")
            for _, x1, y1, x2, y2 in boxes:
                log(f"  (?,?) at ({x1},{y1})-({x2},{y2})")
        finally:
            api.End()

        if len(boxes) < 3:
            log("ERROR: Could not find 3 character positions")
            sys.exit(1)

        # Take the 3 most confident/appropriate boxes
        # Sort by x position, take first 3
        boxes.sort(key=lambda b: b[1])  # sort by x
        boxes = boxes[:3]

        log(f"\nTop 3 boxes (left-to-right):")
        for text, x1, y1, x2, y2 in boxes:
            log(f"  '{text}' at ({x1},{y1})-({x2},{y2})")

        # Strategy: best-effort matching
        # For each expected char, check if it appears in any box
        box_indices = list(range(len(boxes)))
        assigned_boxes = {}  # expected_char -> box_index
        used_indices = set()
        unmatched_chars = []

        for ch in expected_chars:
            found = False
            for idx in box_indices:
                if idx in used_indices:
                    continue
                # Check if this char is in the recognized text
                # Also check if the recognized text contains only this char
                recognized = boxes[idx][0]
                if ch == recognized or (len(recognized) == 1 and ch == recognized):
                    assigned_boxes[ch] = idx
                    used_indices.add(idx)
                    found = True
                    break
            if not found:
                unmatched_chars.append(ch)

        log(f"\nRecognition matching:")
        for ch, idx in assigned_boxes.items():
            log(f"  '{ch}' matched to box[{idx}] at ({boxes[idx][1]},{boxes[idx][2]})")
        if unmatched_chars:
            log(f"  Unmatched chars: {unmatched_chars}")

        # Assign remaining boxes to unmatched chars
        remaining_indices = [i for i in box_indices if i not in used_indices]
        for ch, idx in zip(unmatched_chars, remaining_indices):
            assigned_boxes[ch] = idx
            log(f"  '{ch}' assigned to box[{idx}] (remaining)")

        # If still unmatched (shouldn't happen with 3 boxes and 3 chars)
        if len(assigned_boxes) < 3:
            log("WARNING: Not all chars matched, falling back to position-based")
            for i, (ch, (_, x1, y1, x2, y2)) in enumerate(
                zip(expected_chars, boxes[:3])
            ):
                assigned_boxes[ch] = i

        # Build result in expected order
        result_boxes = []
        for ch in expected_chars:
            idx = assigned_boxes.get(ch, 0)
            result_boxes.append(boxes[idx][1:5])

        log(f"\nFinal click order:")
        for i, ch in enumerate(expected_chars):
            x1, y1, x2, y2 = result_boxes[i]
            log(f"  Click {i+1}: '{ch}' at ({x1},{y1})-({x2},{y2})")

        return result_boxes


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

    try:
        img = Image.open(image_path)
        iw, ih = img.size
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

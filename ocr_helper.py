#!/usr/bin/env python3
import sys
import os
import ddddocr
from PIL import Image

def log(msg):
    print(msg, file=sys.stderr, flush=True)

def main():
    if len(sys.argv) < 3:
        log("ERROR: Missing arguments. Usage: ocr_helper.py <image_path> <expected_chars>")
        sys.exit(1)

    image_path = sys.argv[1]
    expected_chars = sys.argv[2].strip().split()

    if not os.path.exists(image_path):
        log(f"ERROR: Image not found: {image_path}")
        sys.exit(1)

    try:
        log(f"--- Starting Optimized OCR Processing for {image_path} ---")

        det = ddddocr.DdddOcr(det=True, show_ad=False)

        with open(image_path, 'rb') as f:
            img_bytes = f.read()

        bboxes = det.detection(img_bytes)
        log(f"Neural Network detected {len(bboxes)} character blocks.")

        img = Image.open(image_path)
        iw, ih = img.size

        bboxes.sort(key=lambda box: box[0])

        matched_coords = []

        if len(bboxes) < len(expected_chars):
            log("WARNING: Detection found fewer boxes than expected. Triggering fallback average splitting.")
            n = len(expected_chars)
            for i in range(n):
                cx = (i + 0.5) / n
                cy = 0.5
                matched_coords.append((cx, cy))
        else:
            for box in bboxes[:len(expected_chars)]:
                x1, y1, x2, y2 = box
                cx_abs = (x1 + x2) / 2.0
                cy_abs = (y1 + y2) / 2.0
                matched_coords.append((cx_abs / iw, cy_abs / ih))

        for cx, cy in matched_coords:
            print(f"{cx:.4f},{cy:.4f}")

        log("Match successfully completed.")
        for i, (cx, cy) in enumerate(matched_coords):
            log(f" -> Target [{expected_chars[i]}]: Click Pos = ({cx:.4f}, {cy:.4f})")
        log("--- OCR Helper Finished ---")

    except Exception as e:
        log(f"FATAL ERROR in Python OCR Layer: {str(e)}")
        sys.exit(1)

if __name__ == "__main__":
    main()

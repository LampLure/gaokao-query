#!/usr/bin/env python3
import sys
import os
import ddddocr
from PIL import Image, ImageEnhance, ImageFilter

def log(msg):
    print(msg, file=sys.stderr, flush=True)

def preprocess(img):
    enh = ImageEnhance.Contrast(img)
    img = enh.enhance(1.3)
    enh = ImageEnhance.Sharpness(img)
    img = enh.enhance(1.5)
    return img

def filter_boxes(bboxes, iw, ih, n):
    valid = []
    for box in bboxes:
        x1, y1, x2, y2 = box
        bw, bh = x2 - x1, y2 - y1
        if bw < 10 or bh < 10:
            continue
        if bw > iw * 0.5 or bh > ih * 0.8:
            continue
        valid.append(box)
    valid.sort(key=lambda b: b[0])
    return valid[:n]

def detect(img_bytes, use_beta):
    det = ddddocr.DdddOcr(det=True, beta=use_beta, show_ad=False)
    return det.detection(img_bytes)

def main():
    if len(sys.argv) < 3:
        log("ERROR: Missing arguments. Usage: ocr_helper.py <image_path> <expected_chars>")
        sys.exit(1)

    image_path = sys.argv[1]
    expected_chars = sys.argv[2].strip().split()
    n = len(expected_chars)

    if not os.path.exists(image_path):
        log(f"ERROR: Image not found: {image_path}")
        sys.exit(1)

    try:
        log(f"--- OCR Processing: {image_path} ({n} chars) ---")

        with open(image_path, 'rb') as f:
            raw_bytes = f.read()

        bboxes = detect(raw_bytes, use_beta=True)

        if len(bboxes) < n:
            log(f"Beta model found {len(bboxes)} boxes, retrying with default model...")
            bboxes = detect(raw_bytes, use_beta=False)

        if len(bboxes) < n:
            log(f"Default model also insufficient ({len(bboxes)}), retrying with preprocessed image...")
            img = Image.open(image_path)
            if img.mode == 'RGBA':
                img = img.convert('RGB')
            processed = preprocess(img)
            processed_path = image_path + "_processed.png"
            processed.save(processed_path)
            with open(processed_path, 'rb') as f:
                bboxes = detect(f.read(), use_beta=True)
            os.remove(processed_path)

        log(f"Detection result: {len(bboxes)} character boxes.")

        img = Image.open(image_path)
        iw, ih = img.size

        filtered = filter_boxes(bboxes, iw, ih, n)
        log(f"After filtering: {len(filtered)} boxes.")

        if len(filtered) < n:
            log(f"WARNING: Only {len(filtered)} valid boxes for {n} chars. Using fallback.")
            cx_list = [(i + 0.5) / n for i in range(n)]
            cy_list = [0.5] * n
        else:
            cx_list = [(b[0] + b[2]) / 2.0 / iw for b in filtered]
            cy_list = [(b[1] + b[3]) / 2.0 / ih for b in filtered]

        for cx, cy in zip(cx_list, cy_list):
            print(f"{cx:.4f},{cy:.4f}")

        log("--- OCR Completed ---")
        for i, (cx, cy) in enumerate(zip(cx_list, cy_list)):
            log(f"  [{expected_chars[i] if i < n else '?'}] ({cx:.4f}, {cy:.4f})")

    except Exception as e:
        log(f"FATAL ERROR: {str(e)}")
        sys.exit(1)

if __name__ == "__main__":
    main()

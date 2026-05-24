#!/usr/bin/env python3
import sys
import os
import threading
import ddddocr
from PIL import Image, ImageEnhance, ImageFilter

KILL_TIMER = None

def start_kill_timer(seconds=25):
    global KILL_TIMER
    def kill():
        os._exit(1)
    KILL_TIMER = threading.Timer(seconds, kill)
    KILL_TIMER.daemon = True
    KILL_TIMER.start()

def stop_kill_timer():
    global KILL_TIMER
    if KILL_TIMER:
        KILL_TIMER.cancel()

def log(msg):
    print(msg, file=sys.stderr, flush=True)

def preprocess(img):
    if img.mode == 'RGBA':
        img = img.convert('RGB')
    enh = ImageEnhance.Contrast(img)
    img = enh.enhance(1.4)
    enh = ImageEnhance.Sharpness(img)
    img = enh.enhance(2.0)
    img = img.filter(ImageFilter.SHARPEN)
    return img

def filter_boxes(bboxes, iw, ih):
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
    return valid

def try_full_ocr(ocr, img_bytes, expected_set):
    for use_png_fix in [True, False]:
        if use_png_fix:
            result = ocr.classification(img_bytes, png_fix=True).strip()
        else:
            result = ocr.classification(img_bytes).strip()
        result = result.replace(' ', '')
        # Filter to keep only CJK characters
        chars = [c for c in result if '\u4e00' <= c <= '\u9fff']
        log(f"  全图OCR(png_fix={use_png_fix}): raw=[{result}] cjk={chars}")
        if len(chars) == 3 and set(chars) == expected_set:
            return chars
    return None

def detect(img_bytes, use_beta):
    det = ddddocr.DdddOcr(det=True, beta=use_beta, show_ad=False)
    return det.detection(img_bytes)

def main():
    if len(sys.argv) < 3:
        log("ERROR: Missing arguments. Usage: ocr_helper.py <image_path> <expected_chars> [instance_id]")
        sys.exit(1)

    image_path = sys.argv[1]
    expected_chars = sys.argv[2].strip().split()
    instance_id = sys.argv[3] if len(sys.argv) > 3 else "0"
    tmp_dir = f"/tmp/gaokao-captcha-{instance_id}"
    os.makedirs(tmp_dir, exist_ok=True)

    n = len(expected_chars)
    expected_set = set(expected_chars)

    if not os.path.exists(image_path):
        log(f"ERROR: Image not found: {image_path}")
        sys.exit(1)

    start_kill_timer(25)

    try:
        log(f"--- OCR Processing: {image_path} ({n} chars: {expected_chars}) ---")

        with open(image_path, 'rb') as f:
            raw_bytes = f.read()

        # Detection (use preprocessed image if needed)
        bboxes = detect(raw_bytes, use_beta=True)
        if len(bboxes) < n:
            log(f"Beta found {len(bboxes)} boxes, trying default...")
            bboxes = detect(raw_bytes, use_beta=False)
        if len(bboxes) < n:
            log(f"Default insufficient ({len(bboxes)}), trying enhanced image...")
            img = Image.open(image_path)
            processed = preprocess(img)
            proc_path = os.path.join(tmp_dir, "proc.png")
            processed.save(proc_path)
            with open(proc_path, 'rb') as f:
                bboxes = detect(f.read(), use_beta=True)

        log(f"Detection: {len(bboxes)} boxes.")

        img = Image.open(image_path)
        iw, ih = img.size
        filtered = filter_boxes(bboxes, iw, ih)
        log(f"After filter: {len(filtered)} boxes for {n} chars.")

        if len(filtered) < n:
            log("WARNING: Not enough boxes, using fallback split.")
            for i in range(n):
                cx = (i + 0.5) / n
                print(f"{cx:.4f},{0.5:.4f}")
                log(f"  [{expected_chars[i]}] ({cx:.4f}, {0.5:.4f})")
            stop_kill_timer()
            return

        bboxes = filtered[:n]

        # Strategy 1: Full-image OCR (try both default and beta model)
        ocr_default = ddddocr.DdddOcr(show_ad=False)
        ocr_beta = ddddocr.DdddOcr(beta=True, show_ad=False)

        visual_chars = try_full_ocr(ocr_default, raw_bytes, expected_set)
        if not visual_chars:
            log("  尝试Beta模型全图OCR...")
            visual_chars = try_full_ocr(ocr_beta, raw_bytes, expected_set)
        ocr = ocr_beta  # prefer beta for single-char too

        if visual_chars:
            log(f"  ✓ 全图OCR视觉顺序: {visual_chars}")
            box_order = [visual_chars.index(ch) for ch in expected_chars]
            for i, ch in enumerate(expected_chars):
                b = bboxes[box_order[i]]
                cx = (b[0] + b[2]) / 2.0 / iw
                cy = (b[1] + b[3]) / 2.0 / ih
                log(f"  → 第{i+1}次点击 [{ch}] ({cx:.4f}, {cy:.4f})")
                print(f"{cx:.4f},{cy:.4f}")
            log("--- OCR Completed (full image) ---")
            stop_kill_timer()
            return

        # Strategy 2: Try single-char recognition with both models
        log("  全图OCR不匹配，尝试单字识别...")
        char_to_pos = {}
        for i, box in enumerate(bboxes):
            x1, y1, x2, y2 = box
            # Crop with slight margin for better recognition
            margin = 5
            crop = img.crop((max(0,x1-margin), max(0,y1-margin),
                           min(iw,x2+margin), min(ih,y2+margin)))
            crop_path = os.path.join(tmp_dir, f"char_{i}.png")
            crop.save(crop_path)
            with open(crop_path, 'rb') as f:
                ch = ocr.classification(f.read()).strip()
            if ch and ch not in char_to_pos:
                cx = (x1 + x2) / 2.0 / iw
                cy = (y1 + y2) / 2.0 / ih
                char_to_pos[ch] = (cx, cy)
                log(f"  单字识别: [{ch}] -> ({cx:.4f}, {cy:.4f})")

        # Build output in expected order
        fallback_list = [((b[0]+b[2])/2.0/iw, (b[1]+b[3])/2.0/ih) for b in bboxes]
        output = []
        fb_idx = 0
        for ch in expected_chars:
            if ch in char_to_pos:
                output.append(char_to_pos[ch])
            else:
                pos = fallback_list[fb_idx] if fb_idx < len(fallback_list) else (0.5, 0.5)
                log(f"  WARNING: [{ch}] 未识别，使用框{fb_idx+1} ({pos[0]:.4f},{pos[1]:.4f})")
                output.append(pos)
                fb_idx += 1

        for i, ch in enumerate(expected_chars):
            log(f"  → 第{i+1}次点击 [{ch}] ({output[i][0]:.4f}, {output[i][1]:.4f})")
            print(f"{output[i][0]:.4f},{output[i][1]:.4f}")

        log("--- OCR Completed ---")
        stop_kill_timer()

    except Exception as e:
        log(f"FATAL ERROR: {str(e)}")
        sys.exit(1)

if __name__ == "__main__":
    main()

#!/usr/bin/env python3
import sys
import os
import threading
import ddddocr
from PIL import Image, ImageEnhance

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
    enh = ImageEnhance.Contrast(img)
    img = enh.enhance(1.3)
    enh = ImageEnhance.Sharpness(img)
    img = enh.enhance(1.5)
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

def match_and_reorder(bboxes, img_bytes, ocr, expected_chars, iw, ih, image_path, tmp_dir):
    full_text = ocr.classification(img_bytes, png_fix=True).strip()
    log(f"  全图OCR结果: [{full_text}]")

    visual_chars = list(full_text) if len(full_text) == len(expected_chars) else []
    if visual_chars and set(visual_chars) == set(expected_chars):
        log(f"  视觉顺序: {visual_chars}")
        box_order = [visual_chars.index(ch) for ch in expected_chars]
        result = []
        for idx in box_order:
            if idx < len(bboxes):
                b = bboxes[idx]
                cx = (b[0] + b[2]) / 2.0 / iw
                cy = (b[1] + b[3]) / 2.0 / ih
                result.append((cx, cy))
        if len(result) == len(expected_chars):
            for i, ch in enumerate(expected_chars):
                log(f"  → 第{i+1}次点击 [{ch}] 位置 ({result[i][0]:.4f}, {result[i][1]:.4f})")
            return result

    log("  全图OCR不匹配，降级到单字识别...")
    char_to_pos = {}
    used = set()
    for i, box in enumerate(bboxes):
        x1, y1, x2, y2 = box
        crop = Image.open(image_path).crop((x1, y1, x2, y2))
        crop_path = os.path.join(tmp_dir, f"char_{i}.png")
        crop.save(crop_path)
        with open(crop_path, 'rb') as f:
            result = ocr.classification(f.read())
        ch = result.strip()
        if ch and ch not in used:
            cx = (x1 + x2) / 2.0 / iw
            cy = (y1 + y2) / 2.0 / ih
            char_to_pos[ch] = (cx, cy)
            used.add(ch)
            log(f"  单字识别: [{ch}] -> ({cx:.4f}, {cy:.4f})")

    result = []
    fallback_idx = 0
    fallback_list = [((b[0]+b[2])/2.0/iw, (b[1]+b[3])/2.0/ih) for b in bboxes]
    for ch in expected_chars:
        if ch in char_to_pos:
            result.append(char_to_pos[ch])
        else:
            fb = fallback_list[fallback_idx] if fallback_idx < len(fallback_list) else (0.5, 0.5)
            log(f"  WARNING: 未找到字符 [{ch}]，使用第{fallback_idx+1}个框({fb[0]:.4f},{fb[1]:.4f})")
            result.append(fb)
            fallback_idx += 1

    if len(result) != len(expected_chars):
        log(f"  ERROR: 只匹配到 {len(result)}/{len(expected_chars)} 个字符")
        return None
    for i, ch in enumerate(expected_chars):
        log(f"  → 第{i+1}次点击 [{ch}] 位置 ({result[i][0]:.4f}, {result[i][1]:.4f})")
    return result

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

    if not os.path.exists(image_path):
        log(f"ERROR: Image not found: {image_path}")
        sys.exit(1)

    start_kill_timer(25)

    try:
        log(f"--- OCR Processing: {image_path} ({n} chars: {expected_chars}) ---")

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
            processed_path = os.path.join(tmp_dir, "processed.png")
            processed.save(processed_path)
            with open(processed_path, 'rb') as f:
                bboxes = detect(f.read(), use_beta=True)

        log(f"Detection result: {len(bboxes)} character boxes.")

        img = Image.open(image_path)
        iw, ih = img.size
        filtered = filter_boxes(bboxes, iw, ih)
        log(f"After filtering: {len(filtered)} boxes.")

        if len(filtered) < n:
            log(f"WARNING: Only {len(filtered)} valid boxes for {n} chars. Using fallback.")
            for i in range(n):
                cx = (i + 0.5) / n
                cy = 0.5
                print(f"{cx:.4f},{cy:.4f}")
                log(f"  [{expected_chars[i]}] ({cx:.4f}, {cy:.4f})")
        else:
            ocr = ddddocr.DdddOcr(show_ad=False)
            coords = match_and_reorder(filtered[:n], raw_bytes, ocr, expected_chars, iw, ih, image_path, tmp_dir)
            if coords is None:
                log("ERROR: 字符匹配失败，使用x排序保底")
                coords = [((b[0]+b[2])/2.0/iw, (b[1]+b[3])/2.0/ih) for b in filtered[:n]]
            for cx, cy in coords:
                print(f"{cx:.4f},{cy:.4f}")

        log("--- OCR Completed ---")

        stop_kill_timer()

    except Exception as e:
        log(f"FATAL ERROR: {str(e)}")
        sys.exit(1)

if __name__ == "__main__":
    main()

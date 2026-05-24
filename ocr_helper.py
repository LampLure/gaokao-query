#!/usr/bin/env python3
import sys
import os
import itertools
import numpy as np
from PIL import Image, ImageFilter, ImageDraw, ImageFont

TEMPLATE_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "chars")
os.makedirs(TEMPLATE_DIR, exist_ok=True)

FONT_PATHS = [
    "/usr/share/fonts/opentype/noto/NotoSerifCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Medium.ttc",
    "/usr/share/fonts/truetype/arphic/uming.ttc",
]
COMPARE_SIZE = 48
SSIM_THRESHOLD = 0.3


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def _flood_fill(dilated, min_count=15, min_size=8, max_w_ratio=0.4, max_h_ratio=0.5):
    w, h = dilated.size
    iw, ih = w, h
    visited = [[False] * w for _ in range(h)]
    components = []

    for y in range(h):
        for x in range(w):
            if visited[y][x]:
                continue
            if dilated.getpixel((x, y)) != 0:
                visited[y][x] = True
                continue

            stack = [(x, y)]
            visited[y][x] = True
            min_x = max_x = x
            min_y = max_y = y
            count = 0

            while stack:
                cx, cy = stack.pop()
                count += 1
                min_x = min(min_x, cx)
                max_x = max(max_x, cx)
                min_y = min(min_y, cy)
                max_y = max(max_y, cy)
                for nx, ny in [(cx-1,cy),(cx+1,cy),(cx,cy-1),(cx,cy+1),
                               (cx-1,cy-1),(cx+1,cy-1),(cx-1,cy+1),(cx+1,cy+1)]:
                    if 0 <= nx < w and 0 <= ny < h and not visited[ny][nx]:
                        if dilated.getpixel((nx, ny)) == 0:
                            visited[ny][nx] = True
                            stack.append((nx, ny))

            cw, ch = max_x - min_x + 1, max_y - min_y + 1
            if count >= min_count and cw >= min_size and ch >= min_size and cw <= iw * max_w_ratio and ch <= ih * max_h_ratio:
                components.append((min_x, min_y, max_x, max_y, count))

    return components


def _pick_top3(components):
    if len(components) < 3:
        return None
    components.sort(key=lambda c: -c[4])
    top3 = components[:3]
    top3.sort(key=lambda c: c[0])
    return [(x1, y1, x2, y2) for x1, y1, x2, y2, _ in top3]


def find_char_boxes(image_path):
    img = Image.open(image_path)
    if img.mode == 'RGBA':
        img = img.convert('RGB')
    gray = img.convert('L')
    iw, ih = img.size
    log(f"Image size: {iw}x{ih}")

    pixels = sorted(gray.get_flattened_data())
    threshold = min(pixels[len(pixels) * 20 // 100], 160)
    log(f"Threshold: {threshold}")

    bw = gray.point(lambda x: 0 if x < threshold else 255)
    bw.save("/tmp/gaokao_bw.png")
    log("Saved binarized image to /tmp/gaokao_bw.png")

    dilate_sizes = [5, 3, 7, 1]
    for ksize in dilate_sizes:
        if ksize == 1:
            dilated = bw
        else:
            dilated = bw.filter(ImageFilter.MinFilter(ksize))
        components = _flood_fill(dilated)
        log(f"Dilate {ksize}x{ksize}: found {len(components)} components")
        for x1, y1, x2, y2, sz in components:
            log(f"  ({x1},{y1})-({x2},{y2})  size={x2-x1}x{y2-y1}  pixels={sz}")
        result = _pick_top3(components)
        if result is not None:
            log(f"\nSelected 3 components (left-to-right, dilation={ksize}):")
            for x1, y1, x2, y2 in result:
                log(f"  ({x1},{y1})-({x2},{y2})")
            return result

    log("ERROR: Could not find 3 character components with any dilation setting")
    sys.exit(1)


def to_square_gray(img, target_size, bg=255):
    w, h = img.size
    side = max(w, h)
    sq = Image.new('L', (side, side), bg)
    sq.paste(img, ((side - w) // 2, (side - h) // 2))
    return np.array(sq.resize((target_size, target_size), Image.LANCZOS), dtype=np.float32)


def get_cached_template(char):
    path = os.path.join(TEMPLATE_DIR, f"{char}.png")
    if os.path.exists(path):
        return Image.open(path).convert('L')
    return None


def save_template(char, img):
    path = os.path.join(TEMPLATE_DIR, f"{char}.png")
    img.save(path)
    log(f"  Saved template '{char}' ({img.size[0]}x{img.size[1]}) to {path}")


def render_font_img(char):
    results = []
    for fp in FONT_PATHS:
        try:
            canvas = Image.new('L', (COMPARE_SIZE * 2, COMPARE_SIZE * 2), 255)
            draw = ImageDraw.Draw(canvas)
            font = ImageFont.truetype(fp, COMPARE_SIZE)
            draw.text((10, 10), char, font=font, fill=0)
            arr = np.array(canvas)
            ys, xs = np.where(arr < 128)
            if len(xs) < 10:
                continue
            x1, x2 = xs.min(), xs.max()
            y1, y2 = ys.min(), ys.max()
            crop = canvas.crop((x1, y1, x2 + 1, y2 + 1))
            dark_ratio = (arr < 128).sum() / arr.size
            results.append((dark_ratio, crop))
        except Exception:
            continue
    if not results:
        raise RuntimeError(f"No font available to render '{char}'")
    results.sort(key=lambda r: -r[0])
    return results[0][1]


def binarize(arr, threshold=128):
    return (arr < threshold).astype(np.float32)


def jaccard_score(a, b):
    ma, mb = binarize(a), binarize(b)
    inter = (ma * mb).sum()
    union = ((ma + mb) > 0).sum()
    return inter / union if union > 0 else 0


def ncc_score(a, b):
    a_m = a - a.mean()
    b_m = b - b.mean()
    denom = np.sqrt((a_m ** 2).sum() * (b_m ** 2).sum())
    return (a_m * b_m).sum() / denom if denom > 0 else 0


def ssim_score(a, b):
    C1 = (0.01 * 255) ** 2
    C2 = (0.03 * 255) ** 2
    mu1, mu2 = a.mean(), b.mean()
    sigma1_sq = ((a - mu1) ** 2).mean()
    sigma2_sq = ((b - mu2) ** 2).mean()
    sigma12 = ((a - mu1) * (b - mu2)).mean()
    num = (2 * mu1 * mu2 + C1) * (2 * sigma12 + C2)
    den = (mu1 ** 2 + mu2 ** 2 + C1) * (sigma1_sq + sigma2_sq + C2)
    return num / den


def combined_score(a, b):
    return max(ssim_score(a, b), jaccard_score(a, b))


def _score_matrix(gray, boxes, expected_chars, template_source):
    n = len(expected_chars)
    scores = np.zeros((n, n))
    for i, ch in enumerate(expected_chars):
        if template_source == 'cached':
            tmpl = get_cached_template(ch)
        else:
            tmpl = render_font_img(ch)
        tmpl_arr = to_square_gray(tmpl, COMPARE_SIZE)
        for j, (x1, y1, x2, y2) in enumerate(boxes):
            crop = gray.crop((x1, y1, x2, y2))
            crop_arr = to_square_gray(crop, COMPARE_SIZE)
            scores[i, j] = combined_score(crop_arr, tmpl_arr)
    return scores


def _best_assignment(scores, expected_chars):
    n = len(expected_chars)
    best_perm = None
    best_total = -1
    for perm in itertools.permutations(range(n)):
        total = sum(scores[i, perm[i]] for i in range(n))
        if total > best_total:
            best_total = total
            best_perm = perm
    assigned = {}
    for i, j in enumerate(best_perm):
        assigned[expected_chars[i]] = j
    return assigned, best_total / n


def match_characters(boxes, image_path, expected_chars):
    gray = Image.open(image_path).convert('L')
    log(f"\nTemplate matching:")

    n = len(expected_chars)
    all_cached = all(get_cached_template(ch) is not None for ch in expected_chars)

    # Compute scores using font-rendered templates (always available)
    font_scores = _score_matrix(gray, boxes, expected_chars, 'font')
    log(f"Font-rendered scores:")
    for i, ch in enumerate(expected_chars):
        for j in range(n):
            log(f"  '{ch}' vs box[{j}]: match={font_scores[i, j]:.4f}")

    font_assigned, font_avg = _best_assignment(font_scores, expected_chars)

    # If all cached templates exist, also compute with them
    if all_cached:
        cache_scores = _score_matrix(gray, boxes, expected_chars, 'cached')
        log(f"Cached template scores:")
        for i, ch in enumerate(expected_chars):
            for j in range(n):
                log(f"  '{ch}' vs box[{j}]: match={cache_scores[i, j]:.4f}")
        cache_assigned, cache_avg = _best_assignment(cache_scores, expected_chars)

        # Use cached if it's clearly better; otherwise prefer font (unbiased)
        if cache_avg > max(font_avg, 0.5):
            assigned = cache_assigned
            avg = cache_avg
            log(f"\nUsing cached templates (avg score={avg:.4f})")
        else:
            assigned = font_assigned
            avg = font_avg
            log(f"\nUsing font-rendered templates (avg score={avg:.4f})")
    else:
        assigned = font_assigned
        avg = font_avg
        log(f"\nFont-rendered assignment (avg score={avg:.4f}):")

    for ch, j in assigned.items():
        log(f"  '{ch}' → box[{j}] (score={font_scores[expected_chars.index(ch), j]:.4f})")

    if avg < SSIM_THRESHOLD:
        log(f"WARNING: Low score ({avg:.4f}), falling back to left-to-right")
        assigned = {}
        for i, ch in enumerate(expected_chars):
            assigned[ch] = i

    if avg >= SSIM_THRESHOLD:
        for ch, j in assigned.items():
            x1, y1, x2, y2 = boxes[j]
            crop = gray.crop((x1, y1, x2, y2))
            save_template(ch, crop)
    else:
        log("Score too low, not saving templates (to avoid polluting cache)")

    return assigned


def solve(image_path, expected_chars):
    log("--- Character Matching Debug ---")
    log(f"Expected click order: {' → '.join(expected_chars)}")

    boxes = find_char_boxes(image_path)
    if len(boxes) < 3:
        log("ERROR: Could not find 3 character positions")
        sys.exit(1)

    assignments = match_characters(boxes, image_path, expected_chars)

    result = []
    log(f"\nFinal click order:")
    for ch in expected_chars:
        idx = assignments[ch]
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

    log("--- Done ---")


if __name__ == "__main__":
    main()

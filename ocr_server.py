#!/usr/bin/env python3
"""常驻 OCR 服务 — 模型只加载一次，通过 HTTP 接口调用，避免每次验证码重复加载模型权重"""
import io
import base64
import json
import sys
from http.server import HTTPServer, BaseHTTPRequestHandler
import ddddocr
from PIL import Image, ImageEnhance, ImageFilter

# ──────────────────────── 全局模型（只加载一次） ────────────────────────
print("[OCR Server] 正在加载检测模型(beta)...", flush=True)
det_beta = ddddocr.DdddOcr(det=True, beta=True, show_ad=False)
print("[OCR Server] 正在加载检测模型(default)...", flush=True)
det_default = ddddocr.DdddOcr(det=True, beta=False, show_ad=False)
print("[OCR Server] 正在加载识别模型(default)...", flush=True)
ocr_default = ddddocr.DdddOcr(show_ad=False)
print("[OCR Server] 正在加载识别模型(beta)...", flush=True)
ocr_beta = ddddocr.DdddOcr(beta=True, show_ad=False)
print("[OCR Server] ✓ 所有模型加载完成", flush=True)


# ──────────────────────── 图像预处理 ────────────────────────
def preprocess(img):
    if img.mode == 'RGBA':
        img = img.convert('RGB')
    enh = ImageEnhance.Contrast(img)
    img = enh.enhance(1.4)
    enh = ImageEnhance.Sharpness(img)
    img = enh.enhance(2.0)
    img = img.filter(ImageFilter.SHARPEN)
    return img


# ──────────────────────── 检测框过滤与排序 ────────────────────────
def filter_boxes(bboxes, iw, ih, n=3):
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


# ──────────────────────── 全图 OCR（限定候选集） ────────────────────────
# 常见形近字映射，提升识别准确率
CHAR_MAP = {
    '入': '人', '己': '已', '未': '末', '土': '士',
    '日': '曰', '千': '干', '尤': '优', '乃': '及',
    '戊': '戌', '申': '甲', '帅': '师', '辩': '辨',
    '博': '搏', '拨': '拔', '拔': '拨',
}


def try_full_ocr(ocr, img_bytes, expected_chars, expected_set):
    """全图 OCR + 定向候选集映射"""
    for use_png_fix in [True, False]:
        result = ocr.classification(img_bytes, png_fix=use_png_fix).strip().replace(' ', '')
        chars = [c for c in result if '\u4e00' <= c <= '\u9fff']

        # 对每个识别结果，映射到最相似的候选字
        mapped = []
        for c in chars:
            if c in expected_chars:
                mapped.append(c)
            elif c in CHAR_MAP and CHAR_MAP[c] in expected_chars:
                mapped.append(CHAR_MAP[c])

        if len(mapped) == len(expected_chars) and set(mapped) == expected_set:
            return mapped
    return None


# ──────────────────────── 颜色快速定位（仅适用于背景灰色+汉字有色的验证码） ────────────────────────
def fast_locate_by_color(img, n=3):
    """利用验证码汉字颜色与灰色背景的差异，快速定位汉字区域
    适用于：汉字颜色鲜明、背景灰色的验证码（如湖北省高考查询验证码）
    返回：[(cx, cy), ...] 归一化坐标，如果定位失败返回 None"""
    try:
        import numpy as np
    except ImportError:
        return None

    if img.mode != 'RGB':
        img = img.convert('RGB')
    iw, ih = img.size

    arr = np.array(img)

    # 灰色背景像素检测：RGB 三通道差值小且亮度高
    r, g, b = arr[:, :, 0].astype(int), arr[:, :, 1].astype(int), arr[:, :, 2].astype(int)
    is_gray = (np.abs(r - g) < 25) & (np.abs(g - b) < 25) & (r > 170)
    is_white = (r > 240) & (g > 240) & (b > 240)

    # 有色像素 = 非灰色 & 非白色 & 亮度不太高
    is_colored = ~is_gray & ~is_white & (arr.max(axis=2) < 230)

    # 水平投影
    col_sum = is_colored.sum(axis=0)
    threshold = ih * 0.08

    # 找连续有色区域
    in_region = False
    regions = []
    start = 0
    for x in range(iw):
        if col_sum[x] > threshold and not in_region:
            in_region = True
            start = x
        elif col_sum[x] <= threshold and in_region:
            in_region = False
            regions.append((start, x))
    if in_region:
        regions.append((start, iw))

    if len(regions) != n:
        return None

    points = []
    for x1, x2 in regions:
        cx = round((x1 + x2) / 2.0 / iw, 4)
        cy = 0.5
        points.append((cx, cy))

    return points


# ──────────────────────── 核心识别逻辑 ────────────────────────
def solve_captcha(img_bytes, expected_chars):
    n = len(expected_chars)
    expected_set = set(expected_chars)

    # ── 步骤 0：颜色快速定位（最快路径，<10ms） ──
    img = Image.open(io.BytesIO(img_bytes))
    fast_points = fast_locate_by_color(img, n)

    if fast_points is not None:
        # 颜色定位成功，但还需要确定点击顺序
        # 用全图 OCR 确定顺序
        visual_chars = try_full_ocr(ocr_default, img_bytes, expected_chars, expected_set)
        if not visual_chars:
            visual_chars = try_full_ocr(ocr_beta, img_bytes, expected_chars, expected_set)

        if visual_chars:
            # 按照 expected_chars 的顺序排列 fast_points
            # fast_points 是从左到右的，visual_chars 也是从左到右的
            # 需要建立 visual_chars -> expected_chars 的点击顺序映射
            box_order = [visual_chars.index(ch) for ch in expected_chars]
            result = [fast_points[box_order[i]] for i in range(n)]
            return result, "color+full_ocr"

        # 颜色定位成功但 OCR 无法确定顺序，回退到检测模型

    # ── 步骤 1：检测模型定位 ──
    bboxes = det_beta.detection(img_bytes)
    if len(bboxes) < n:
        bboxes = det_default.detection(img_bytes)

    iw, ih = img.size

    if len(bboxes) < n:
        processed = preprocess(img)
        buf = io.BytesIO()
        processed.save(buf, format='PNG')
        bboxes = det_beta.detection(buf.getvalue())

    filtered = filter_boxes(bboxes, iw, ih, n)

    if len(filtered) < n:
        # 均分兜底
        fallback = [(round((i + 0.5) / n, 4), 0.5) for i in range(n)]
        return fallback, "fallback_split"

    # ── 步骤 2：全图 OCR 确定顺序 ──
    visual_chars = try_full_ocr(ocr_default, img_bytes, expected_chars, expected_set)
    if not visual_chars:
        visual_chars = try_full_ocr(ocr_beta, img_bytes, expected_chars, expected_set)

    if visual_chars:
        box_order = [visual_chars.index(ch) for ch in expected_chars]
        result = []
        for i in range(n):
            b = filtered[box_order[i]]
            cx = round((b[0] + b[2]) / 2.0 / iw, 4)
            cy = round((b[1] + b[3]) / 2.0 / ih, 4)
            result.append((cx, cy))
        return result, "detect+full_ocr"

    # ── 步骤 3：单字识别（兜底） ──
    char_to_pos = {}
    for i, box in enumerate(filtered):
        x1, y1, x2, y2 = box
        margin = 5
        crop = img.crop((max(0, x1 - margin), max(0, y1 - margin),
                         min(iw, x2 + margin), min(ih, y2 + margin)))
        buf = io.BytesIO()
        crop.save(buf, format='PNG')
        ch = ocr_beta.classification(buf.getvalue()).strip()

        # 定向候选集映射
        if ch in expected_chars:
            mapped_ch = ch
        elif ch in CHAR_MAP and CHAR_MAP[ch] in expected_chars:
            mapped_ch = CHAR_MAP[ch]
        else:
            mapped_ch = ch

        if mapped_ch and mapped_ch not in char_to_pos:
            cx = round((x1 + x2) / 2.0 / iw, 4)
            cy = round((y1 + y2) / 2.0 / ih, 4)
            char_to_pos[mapped_ch] = (cx, cy)

    fallback = [((b[0] + b[2]) / 2.0 / iw, (b[1] + b[3]) / 2.0 / ih) for b in filtered]
    result = []
    fb_idx = 0
    for ch in expected_chars:
        if ch in char_to_pos:
            result.append(char_to_pos[ch])
        else:
            result.append(fallback[fb_idx] if fb_idx < len(fallback) else (0.5, 0.5))
            fb_idx += 1

    return result, "detect+single_char"


# ──────────────────────── HTTP 服务 ────────────────────────
class OCRHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        try:
            length = int(self.headers.get('Content-Length', 0))
            body = self.rfile.read(length)
            data = json.loads(body)

            img_b64 = data.get('image', '')
            expected_chars = data.get('expected_chars', [])

            img_bytes = base64.b64decode(img_b64)
            points, strategy = solve_captcha(img_bytes, expected_chars)

            response = json.dumps({
                "points": points,
                "strategy": strategy,
                "debug": f"OK via {strategy}"
            })
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(response.encode())

        except Exception as e:
            error_resp = json.dumps({
                "points": [],
                "strategy": "error",
                "debug": str(e)
            })
            self.send_response(500)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(error_resp.encode())

    def log_message(self, format, *args):
        pass  # 静默日志


if __name__ == '__main__':
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 19999
    server = HTTPServer(('127.0.0.1', port), OCRHandler)
    print(f"[OCR Server] ✓ 监听端口: {port}", flush=True)
    server.serve_forever()

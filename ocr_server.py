#!/usr/bin/env python3
"""常驻 OCR 服务 — 模型只加载一次，通过 HTTP 接口调用，避免每次验证码重复加载模型权重

v2 优化重点：
1. 单字识别优先于全图 OCR（全图 OCR 经常返回错误字符，单字更准）
2. 扩充形近字映射表（CHAR_MAP），覆盖更多常见误识别
3. 多模型交叉验证：单字识别使用 ocr_beta + ocr_default 双模型投票
4. 检测框过滤优化：宽高比、面积过滤，合并重叠框，处理 4框→3框 场景
5. 颜色定位改进：增加垂直投影精度，支持非居中文字位置
6. 全图 OCR 放宽匹配：允许超集匹配+子序列提取，不再要求严格等集
7. 返回 debug 信息更详细，方便排查
"""
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


def preprocess_aggressive(img):
    """更激进的预处理：高对比度+二值化，用于单字识别困难时"""
    if img.mode == 'RGBA':
        img = img.convert('RGB')
    enh = ImageEnhance.Contrast(img)
    img = enh.enhance(2.0)
    enh = ImageEnhance.Sharpness(img)
    img = enh.enhance(3.0)
    img = img.filter(ImageFilter.SHARPEN)
    # 简单二值化：像素亮度 > 阈值 → 白，否则 → 黑
    gray = img.convert('L')
    threshold = 128
    img = gray.point(lambda x: 255 if x > threshold else 0, '1')
    img = img.convert('RGB')
    return img


# ──────────────────────── 检测框过滤与排序 ────────────────────────
def filter_boxes(bboxes, iw, ih, n=3):
    """过滤检测框：去除过小/过大框，合并重叠框，保留最可靠的 n 个"""
    valid = []
    for box in bboxes:
        x1, y1, x2, y2 = box
        bw, bh = x2 - x1, y2 - y1
        if bw < 10 or bh < 10:
            continue
        # 宽高比检查：验证码中的汉字大致是正方形或略高
        ratio = bw / bh if bh > 0 else 999
        if ratio < 0.3 or ratio > 3.0:
            continue
        if bw > iw * 0.5 or bh > ih * 0.8:
            continue
        valid.append(box)

    # 合并重叠框（IoU > 0.5 的框合并为一个）
    valid.sort(key=lambda b: b[0])
    merged = []
    for box in valid:
        x1, y1, x2, y2 = box
        if not merged:
            merged.append(box)
            continue
        px1, py1, px2, py2 = merged[-1]
        # 计算 IoU
        ix1 = max(x1, px1)
        iy1 = max(y1, py1)
        ix2 = min(x2, px2)
        iy2 = min(y2, py2)
        inter = max(0, ix2 - ix1) * max(0, iy2 - iy1)
        area_a = (x2 - x1) * (y2 - y1)
        area_b = (px2 - px1) * (py2 - py1)
        union = area_a + area_b - inter
        iou = inter / union if union > 0 else 0
        if iou > 0.4:
            # 合并：取外接矩形
            merged[-1] = (min(x1, px1), min(y1, py1), max(x2, px2), max(y2, py2))
        else:
            merged.append(box)

    # 如果框数 > n，按面积从大到小排序取前 n（大框更可能是汉字）
    if len(merged) > n:
        merged.sort(key=lambda b: (b[2] - b[0]) * (b[3] - b[1]), reverse=True)
        merged = merged[:n]

    # 最终按 x 坐标从左到右排序
    merged.sort(key=lambda b: b[0])
    return merged[:n]


# ──────────────────────── 形近字映射表（大幅扩充） ────────────────────────
# 双向映射：key → value 表示"如果 OCR 识别出 key，可能实际是 value"
CHAR_MAP = {
    # 笔画相似
    '入': '人', '己': '已', '未': '末', '土': '士',
    '日': '曰', '千': '干', '尤': '优', '乃': '及',
    '戊': '戌', '申': '甲', '帅': '师', '辩': '辨',
    '博': '搏', '拨': '拔', '拔': '拨',
    # 常见 OCR 误识别
    '最': '量', '量': '最', '黑': '墨', '墨': '黑',
    '里': '童', '童': '里', '舍': '答', '答': '舍',
    '大': '太', '太': '大', '犬': '太', '木': '本',
    '本': '木', '米': '木', '禾': '木', '朱': '未',
    '白': '百', '百': '白', '自': '白', '目': '日',
    '月': '日', '田': '由', '由': '田', '甲': '由',
    '电': '由', '生': '土', '王': '玉', '玉': '王',
    '主': '王', '丰': '王', '夫': '天', '天': '夫',
    '元': '无', '无': '元', '开': '井', '井': '开',
    '厂': '广', '广': '厂', '己': '已', '已': '己',
    '巳': '己', '巴': '巳', '孔': '孙', '长': '张',
    '张': '长', '少': '小', '小': '少', '山': '出',
    '出': '山', '上': '下', '下': '上', '中': '串',
    '串': '中', '口': '回', '回': '口', '四': '口',
    '匹': '四', '区': '巨', '巨': '区', '臣': '巨',
    '力': '刀', '刀': '力', '乃': '及', '万': '方',
    '方': '万', '女': '如', '如': '女', '子': '字',
    '字': '子', '学': '字', '文': '齐', '齐': '文',
    '这': '过', '过': '这', '道': '首', '首': '道',
    '贝': '见', '见': '贝', '页': '贝', '贞': '页',
    '占': '古', '古': '占', '舌': '古', '言': '信',
    '信': '言', '计': '认', '认': '计', '让': '话',
    '话': '让', '识': '织', '织': '识',
    # 更多高频误识
    '暖': '眼', '眼': '暖', '睛': '晴', '晴': '睛',
    '村': '林', '林': '村', '杜': '材', '材': '杜',
    '折': '拆', '拆': '折', '诉': '折', '听': '所',
    '所': '听', '新': '亲', '亲': '新', '立': '产',
    '产': '立', '厂': '广', '庆': '广', '床': '广',
    '座': '广', '病': '广', '展': '居', '居': '展',
    '眉': '看', '看': '眉', '省': '看', '看': '着',
    '着': '看', '样': '檬', '檬': '样', '校': '核',
    '核': '校', '检': '验', '验': '检',
}

# 构建反向映射（双向查找）
_REVERSE_CHAR_MAP = {}
for k, v in CHAR_MAP.items():
    if v not in _REVERSE_CHAR_MAP:
        _REVERSE_CHAR_MAP[v] = []
    _REVERSE_CHAR_MAP[v].append(k)


def map_char(ch, expected_chars):
    """将识别字符映射到最可能的候选字符，返回映射后的字符或原字符"""
    if ch in expected_chars:
        return ch
    if ch in CHAR_MAP and CHAR_MAP[ch] in expected_chars:
        return CHAR_MAP[ch]
    # 反向映射查找
    if ch in _REVERSE_CHAR_MAP:
        for rev in _REVERSE_CHAR_MAP[ch]:
            if rev in expected_chars:
                return rev
    # 检查 expected_chars 是否在 CHAR_MAP 中映射到 ch
    for exp in expected_chars:
        if exp in CHAR_MAP and CHAR_MAP[exp] == ch:
            return exp
    return None  # 无法映射


# ──────────────────────── 全图 OCR（放宽匹配） ────────────────────────
def try_full_ocr(ocr, img_bytes, expected_chars, expected_set):
    """全图 OCR + 定向候选集映射（放宽版）
    改进：不再要求 mapped == expected_set 严格等集，
    而是允许从 OCR 结果中提取子序列匹配 expected_chars 的排列顺序
    """
    for use_png_fix in [True, False]:
        result = ocr.classification(img_bytes, png_fix=use_png_fix).strip().replace(' ', '')
        chars = [c for c in result if '\u4e00' <= c <= '\u9fff']

        # 对每个识别结果，映射到最相似的候选字
        mapped = []
        for c in chars:
            mc = map_char(c, expected_chars)
            if mc:
                mapped.append(mc)

        # 严格匹配：映射后恰好是 expected_chars 的排列
        if len(mapped) == len(expected_chars) and set(mapped) == expected_set:
            return mapped

        # 放宽匹配：从 mapped 中提取 expected_chars 的子序列
        if len(mapped) >= len(expected_chars):
            # 尝试按 expected_chars 的顺序，在 mapped 中依次找到每个字
            subseq = []
            idx = 0
            for ch in expected_chars:
                for j in range(idx, len(mapped)):
                    if mapped[j] == ch:
                        subseq.append(ch)
                        idx = j + 1
                        break
            if len(subseq) == len(expected_chars):
                return subseq

    return None


# ──────────────────────── 颜色快速定位 ────────────────────────
def fast_locate_by_color(img, n=3):
    """利用验证码汉字颜色与灰色背景的差异，快速定位汉字区域
    v2: 改进垂直定位精度，不再硬编码 cy=0.5
    """
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
        # v2: 计算实际垂直中心而非硬编码 0.5
        region_colored = is_colored[:, x1:x2]
        if region_colored.any():
            row_sum = region_colored.sum(axis=1)
            y_weighted = (row_sum * np.arange(ih)).sum() / row_sum.sum()
            cy = round(y_weighted / ih, 4)
        else:
            cy = 0.5
        points.append((cx, cy))

    return points


# ──────────────────────── 单字识别（双模型投票） ────────────────────────
def recognize_single_char(img, box, iw, ih, expected_chars):
    """对单个检测框进行字符识别，使用双模型投票+映射
    返回 (mapped_char, (cx, cy)) 或 None
    """
    x1, y1, x2, y2 = box
    cx = round((x1 + x2) / 2.0 / iw, 4)
    cy = round((y1 + y2) / 2.0 / ih, 4)
    margin = 5

    results = []
    for ocr_model in [ocr_beta, ocr_default]:
        # 普通预处理裁剪
        crop = img.crop((max(0, x1 - margin), max(0, y1 - margin),
                         min(iw, x2 + margin), min(ih, y2 + margin)))
        buf = io.BytesIO()
        crop.save(buf, format='PNG')
        ch = ocr_model.classification(buf.getvalue()).strip()
        results.append(ch)

        # 激进预处理裁剪（二值化）
        try:
            crop2 = preprocess_aggressive(img.crop((max(0, x1 - margin), max(0, y1 - margin),
                                                    min(iw, x2 + margin), min(ih, y2 + margin))))
            buf2 = io.BytesIO()
            crop2.save(buf2, format='PNG')
            ch2 = ocr_model.classification(buf2.getvalue()).strip()
            results.append(ch2)
        except Exception:
            pass

    # 投票：如果多个模型一致则高置信度
    from collections import Counter
    vote = Counter(results)
    best_ch, best_count = vote.most_common(1)[0]

    # 尝试映射到候选字
    mapped = map_char(best_ch, expected_chars)
    if mapped:
        return mapped, (cx, cy)

    # 如果最佳结果无法映射，尝试所有结果中能映射的
    for ch in results:
        mapped = map_char(ch, expected_chars)
        if mapped:
            return mapped, (cx, cy)

    # 完全无法映射，返回原始识别结果（用于 debug）
    return best_ch, (cx, cy)


# ──────────────────────── 核心识别逻辑 ────────────────────────
def solve_captcha(img_bytes, expected_chars):
    n = len(expected_chars)
    expected_set = set(expected_chars)
    debug_lines = []

    # ── 步骤 0：颜色快速定位（最快路径，<10ms） ──
    img = Image.open(io.BytesIO(img_bytes))
    fast_points = fast_locate_by_color(img, n)

    if fast_points is not None:
        debug_lines.append(f"颜色定位成功: {[f'({p[0]},{p[1]})' for p in fast_points]}")
        # 颜色定位成功，尝试确定点击顺序
        # 优先使用单字识别（在颜色区域上裁剪识别）
        char_to_point = {}
        for i, (px, py) in enumerate(fast_points):
            # 将归一化坐标转回像素坐标
            iw, ih = img.size
            cx_px = int(px * iw)
            cy_px = int(py * ih)
            # 在颜色区域周围裁剪
            half_w = iw // (n * 2)
            x1 = max(0, cx_px - half_w)
            x2 = min(iw, cx_px + half_w)
            y1 = max(0, cy_px - ih // 3)
            y2 = min(ih, cy_px + ih // 3)

            crop = img.crop((x1, y1, x2, y2))
            buf = io.BytesIO()
            crop.save(buf, format='PNG')

            # 双模型识别
            ch_results = []
            for ocr_model in [ocr_beta, ocr_default]:
                ch = ocr_model.classification(buf.getvalue()).strip()
                ch_results.append(ch)

            # 投票
            from collections import Counter
            vote = Counter(ch_results)
            best_ch, _ = vote.most_common(1)[0]

            mapped = map_char(best_ch, expected_chars)
            if mapped:
                char_to_point[mapped] = (px, py)
                debug_lines.append(f"  颜色区域#{i}: 识别='{best_ch}' → 映射='{mapped}'")

        # 如果所有字都识别出来了
        if all(ch in char_to_point for ch in expected_chars):
            result = [char_to_point[ch] for ch in expected_chars]
            debug_lines.append(f"  ✓ 颜色+单字全部命中")
            return result, "color+single_char"

        # 部分识别成功，用全图 OCR 补充顺序
        visual_chars = try_full_ocr(ocr_default, img_bytes, expected_chars, expected_set)
        if not visual_chars:
            visual_chars = try_full_ocr(ocr_beta, img_bytes, expected_chars, expected_set)

        if visual_chars:
            box_order = [visual_chars.index(ch) for ch in expected_chars]
            result = [fast_points[box_order[i]] for i in range(n)]
            debug_lines.append(f"  颜色定位+全图OCR排序成功")
            return result, "color+full_ocr"

    else:
        debug_lines.append("颜色定位失败")

    # ── 步骤 1：检测模型定位 ──
    bboxes = det_beta.detection(img_bytes)
    debug_lines.append(f"检测(beta): {len(bboxes)}框")
    if len(bboxes) < n:
        bboxes = det_default.detection(img_bytes)
        debug_lines.append(f"检测(default): {len(bboxes)}框")

    iw, ih = img.size

    if len(bboxes) < n:
        processed = preprocess(img)
        buf = io.BytesIO()
        processed.save(buf, format='PNG')
        bboxes = det_beta.detection(buf.getvalue())
        debug_lines.append(f"检测(预处理+beta): {len(bboxes)}框")

    filtered = filter_boxes(bboxes, iw, ih, n)
    debug_lines.append(f"过滤后: {len(filtered)}框")

    if len(filtered) < n:
        # 均分兜底
        fallback = [(round((i + 0.5) / n, 4), 0.5) for i in range(n)]
        debug_lines.append(f"框不足{n}个，均分兜底")
        return fallback, "fallback_split"

    # ── 步骤 2：单字识别优先（v2 核心改进：单字比全图更准） ──
    char_to_pos = {}
    used_boxes = set()
    debug_lines.append("开始单字识别...")

    for i, box in enumerate(filtered):
        result = recognize_single_char(img, box, iw, ih, expected_chars)
        if result:
            mapped_ch, pos = result
            debug_lines.append(f"  框#{i}: 映射='{mapped_ch}' @ ({pos[0]},{pos[1]})")
            # 只有映射到期望字符的才记录（避免误识别覆盖）
            if mapped_ch in expected_chars and mapped_ch not in char_to_pos:
                char_to_pos[mapped_ch] = pos
                used_boxes.add(i)

    # 检查是否所有期望字符都找到了
    all_found = all(ch in char_to_pos for ch in expected_chars)
    if all_found:
        result = [char_to_pos[ch] for ch in expected_chars]
        debug_lines.append(f"  ✓ 单字识别全部命中")
        print(f"[OCR] {', '.join(debug_lines)}", flush=True)
        return result, "detect+single_char"

    # ── 步骤 3：全图 OCR 辅助排序 ──
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
        debug_lines.append(f"  全图OCR排序成功: {visual_chars}")
        print(f"[OCR] {', '.join(debug_lines)}", flush=True)
        return result, "detect+full_ocr"

    # ── 步骤 4：混合策略：单字识别部分命中 + 物理位置兜底 ──
    fallback = [((b[0] + b[2]) / 2.0 / iw, (b[1] + b[3]) / 2.0 / ih) for b in filtered]
    result = []
    fb_idx = 0
    for ch in expected_chars:
        if ch in char_to_pos:
            result.append(char_to_pos[ch])
            debug_lines.append(f"  '{ch}': 单字命中")
        else:
            if fb_idx < len(fallback):
                result.append((round(fallback[fb_idx][0], 4), round(fallback[fb_idx][1], 4)))
                debug_lines.append(f"  '{ch}': 物理位置兜底(框#{fb_idx})")
            else:
                result.append((0.5, 0.5))
                debug_lines.append(f"  '{ch}': 无可用框，中心兜底")
            fb_idx += 1

    debug_lines.append("  混合策略完成")
    print(f"[OCR] {', '.join(debug_lines)}", flush=True)
    return result, "detect+single_char_partial"


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

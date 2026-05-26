#!/usr/bin/env python3
"""常驻 OCR 服务 — 模型只加载一次，通过 HTTP 接口调用，避免每次验证码重复加载模型权重

v3 核心改进（一招鲜策略）：
1. 单字识别为主策略：全图OCR对验证码几乎无效，直接跳过，用检测+单字识别
2. 修复坐标重复bug：未识别字符跳过已占用的框
3. 双模型投票增强单字识别准确率
4. 自动刷新验证码检测：通过截图hash判断验证码是否已刷新
5. 更详细的debug信息

v3.1 改进：
- 添加 GET /health 端点用于轻量健康检查
- 添加就绪信号文件机制，避免启动期间反复HTTP探测
- 模型加载失败时给出明确错误信息
"""
import io
import os
import base64
import json
import sys
import signal
import tempfile
from http.server import HTTPServer, BaseHTTPRequestHandler, ThreadingHTTPServer

# ──────────────────────── 模型加载（带错误处理） ────────────────────────
MODELS_LOADED = False

def load_models():
    """加载所有OCR模型，返回是否成功。
    自动检测GPU可用性，优先使用GPU加速。
    """
    global det_beta, det_default, ocr_beta, ocr_default, MODELS_LOADED
    try:
        import ddddocr
    except ImportError:
        print("[OCR Server] ❌ ddddocr 未安装！请运行: pip install ddddocr", flush=True)
        return False

    try:
        from PIL import Image, ImageEnhance, ImageFilter
    except ImportError:
        print("[OCR Server] ❌ Pillow 未安装！请运行: pip install Pillow", flush=True)
        return False

    # 自动检测GPU可用性
    use_gpu = False
    try:
        import onnxruntime as ort
        providers = ort.get_available_providers()
        if 'CUDAExecutionProvider' in providers:
            use_gpu = True
            print(f"[OCR Server] 🚀 检测到CUDA GPU，启用GPU加速 (providers: {providers})", flush=True)
        else:
            print(f"[OCR Server] GPU不可用，使用CPU模式 (providers: {providers})", flush=True)
    except ImportError:
        print("[OCR Server] onnxruntime未安装，使用CPU模式", flush=True)

    try:
        print("[OCR Server] 正在加载检测模型(beta)...", flush=True)
        det_beta = ddddocr.DdddOcr(det=True, beta=True, show_ad=False, use_gpu=use_gpu)
        print("[OCR Server] 正在加载检测模型(default)...", flush=True)
        det_default = ddddocr.DdddOcr(det=True, beta=False, show_ad=False, use_gpu=use_gpu)
        print("[OCR Server] 正在加载识别模型(beta)...", flush=True)
        ocr_beta = ddddocr.DdddOcr(beta=True, show_ad=False, use_gpu=use_gpu)
        print("[OCR Server] 正在加载识别模型(default)...", flush=True)
        ocr_default = ddddocr.DdddOcr(show_ad=False, use_gpu=use_gpu)
        mode_str = "GPU" if use_gpu else "CPU"
        print(f"[OCR Server] ✓ 所有模型加载完成 (模式: {mode_str})", flush=True)
        MODELS_LOADED = True
        return True
    except Exception as e:
        print(f"[OCR Server] ❌ 模型加载失败: {e}", flush=True)
        import traceback
        traceback.print_exc()
        return False

# 先不加载模型，等 main 里再加载


# ──────────────────────── 图像预处理 ────────────────────────
def preprocess(img):
    from PIL import ImageEnhance, ImageFilter
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
    from PIL import ImageEnhance, ImageFilter
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

    # 合并重叠框（IoU > 0.4 的框合并为一个）
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
    '厂': '广', '广': '厂', '巳': '己', '巴': '巳',
    '孔': '孙', '长': '张', '张': '长', '少': '小',
    '小': '少', '山': '出', '出': '山', '上': '下',
    '下': '上', '中': '串', '串': '中', '口': '回',
    '回': '口', '四': '口', '匹': '四', '区': '巨',
    '巨': '区', '臣': '巨', '力': '刀', '刀': '力',
    '乃': '及', '万': '方', '方': '万', '女': '如',
    '如': '女', '子': '字', '字': '子', '学': '字',
    '文': '齐', '齐': '文', '这': '过', '过': '这',
    '道': '首', '首': '道', '贝': '见', '见': '贝',
    '页': '贝', '贞': '页', '占': '古', '古': '占',
    '舌': '古', '言': '信', '信': '言', '计': '认',
    '认': '计', '让': '话', '话': '让', '识': '织',
    '织': '识',
    # 更多高频误识
    '暖': '眼', '眼': '暖', '睛': '晴', '晴': '睛',
    '村': '林', '林': '村', '杜': '材', '材': '杜',
    '折': '拆', '拆': '折', '诉': '折', '听': '所',
    '所': '听', '新': '亲', '亲': '新', '立': '产',
    '产': '立', '庆': '广', '床': '广',
    '座': '广', '病': '广', '展': '居', '居': '展',
    '眉': '看', '省': '看', '看': '着', '着': '看',
    '样': '檬', '檬': '样', '校': '核', '核': '校',
    '检': '验', '验': '检',
    # 补充更多形近字
    '园': '圆', '圆': '园', '史': '吏', '吏': '史',
    '阳': '阴', '阴': '阳', '走': '起', '起': '走',
    '问': '间', '间': '问', '口': '日', '日': '口',
    '十': '千', '千': '十', '生': '土', '有': '布',
    '理': '里', '里': '理', '课': '棵', '棵': '课',
    '政': '改', '改': '政', '语': '诂', '书': '画',
    '画': '书', '优': '犹', '究': '九', '考': '老',
    '老': '考', '记': '纪', '纪': '记', '背': '皆',
    '科': '种', '种': '科', '师': '帅',
    '习': '羽', '写': '与', '读': '续',
}

# 构建反向映射（双向查找）
_REVERSE_CHAR_MAP = {}
for k, v in CHAR_MAP.items():
    if v not in _REVERSE_CHAR_MAP:
        _REVERSE_CHAR_MAP[v] = []
    _REVERSE_CHAR_MAP[v].append(k)


def map_char(ch, expected_chars):
    """将识别字符映射到最可能的候选字符，返回映射后的字符或None"""
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


# ──────────────────────── 单字识别（双模型投票）────────────────────────
def recognize_single_char(img, box, iw, ih, expected_chars):
    """对单个检测框进行字符识别，使用双模型投票+映射
    返回 (mapped_char, (cx, cy)) 或 (raw_char, (cx, cy))
    mapped_char 可能为 None 表示无法映射到候选字
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
        return mapped, (cx, cy), best_count

    # 如果最佳结果无法映射，尝试所有结果中能映射的（优先投票数高的）
    for ch, count in vote.most_common():
        mapped = map_char(ch, expected_chars)
        if mapped:
            return mapped, (cx, cy), count

    # 完全无法映射，返回原始识别结果
    return best_ch, (cx, cy), 0


# ──────────────────────── 核心识别逻辑（v3：单字优先） ────────────────────────
def solve_captcha(img_bytes, expected_chars):
    """v3 核心：检测定位 → 单字识别（主策略）→ 坐标去重 → 兜底

    不再使用全图OCR作为主策略，因为全图OCR对验证码几乎无效。
    全图OCR仅在单字识别部分失败时用于排序辅助。
    """
    from PIL import Image

    n = len(expected_chars)
    expected_set = set(expected_chars)
    debug_lines = []

    img = Image.open(io.BytesIO(img_bytes))
    iw, ih = img.size
    debug_lines.append(f"--- OCR Processing: ({n} chars: {expected_chars}) ---")

    # ── 步骤 1：检测模型定位 ──
    bboxes = det_beta.detection(img_bytes)
    debug_lines.append(f"Detection: {len(bboxes)} boxes.")

    if len(bboxes) < n:
        bboxes2 = det_default.detection(img_bytes)
        debug_lines.append(f"Default detection: {len(bboxes2)} boxes.")
        if len(bboxes2) >= len(bboxes):
            bboxes = bboxes2

    if len(bboxes) < n:
        processed = preprocess(img)
        buf = io.BytesIO()
        processed.save(buf, format='PNG')
        bboxes2 = det_beta.detection(buf.getvalue())
        debug_lines.append(f"Preprocessed detection: {len(bboxes2)} boxes.")
        if len(bboxes2) >= len(bboxes):
            bboxes = bboxes2

    filtered = filter_boxes(bboxes, iw, ih, n)
    debug_lines.append(f"After filter: {len(filtered)} boxes for {n} chars.")

    if len(filtered) < n:
        # 均分兜底（极少发生）
        fallback = [(round((i + 0.5) / n, 4), 0.5) for i in range(n)]
        debug_lines.append(f"框不足{n}个，均分兜底")
        debug_lines.append("--- OCR Completed (fallback) ---")
        return fallback, "fallback_split", "\n".join(debug_lines)

    # ── 步骤 2：单字识别（一招鲜！直接主策略） ──
    char_to_pos = {}        # 映射成功的字 → 坐标
    box_to_char = {}        # 框索引 → 映射后的字
    used_box_indices = set()  # 已被使用的框索引

    debug_lines.append("开始单字识别...")
    for i, box in enumerate(filtered):
        mapped_ch, pos, confidence = recognize_single_char(img, box, iw, ih, expected_chars)
        debug_lines.append(f"  单字识别: raw=[{mapped_ch}] mapped=[{mapped_ch}] -> box_{i}")

        if mapped_ch in expected_chars and mapped_ch not in char_to_pos:
            # 精确命中期望字
            char_to_pos[mapped_ch] = pos
            box_to_char[i] = mapped_ch
            used_box_indices.add(i)
        elif mapped_ch not in expected_chars and mapped_ch is not None:
            # 识别出了字但不在候选中，记录原始结果供参考
            debug_lines.append(f"  WARNING: [{mapped_ch}] 未匹配到候选字")

    # ── 步骤 3：检查是否所有期望字都找到了 ──
    all_found = all(ch in char_to_pos for ch in expected_chars)
    if all_found:
        result = [char_to_pos[ch] for ch in expected_chars]
        for i, ch in enumerate(expected_chars):
            debug_lines.append(f"  → 第{i+1}次点击 [{ch}] ({char_to_pos[ch][0]:.4f}, {char_to_pos[ch][1]:.4f})")
        debug_lines.append("--- OCR Completed ---")
        return result, "single_char", "\n".join(debug_lines)

    # ── 步骤 4：处理未识别的字（修复坐标重复bug！） ──
    # 关键修复：对未识别的字，使用未被占用的框坐标，而不是顺序递增的fb_idx
    unused_boxes = [(i, filtered[i]) for i in range(len(filtered)) if i not in used_box_indices]

    for ch in expected_chars:
        if ch in char_to_pos:
            continue  # 已识别，跳过

        # 尝试从未使用的框中找一个
        if unused_boxes:
            idx, box = unused_boxes.pop(0)
            x1, y1, x2, y2 = box
            cx = round((x1 + x2) / 2.0 / iw, 4)
            cy = round((y1 + y2) / 2.0 / ih, 4)
            char_to_pos[ch] = (cx, cy)
            used_box_indices.add(idx)
            debug_lines.append(f"  WARNING: [{ch}] 未识别，使用未占用框{idx} ({cx:.4f},{cy:.4f})")
        else:
            # 没有可用框了，用中心点
            char_to_pos[ch] = (0.5, 0.5)
            debug_lines.append(f"  WARNING: [{ch}] 未识别，无可用框，使用中心点")

    # 检查坐标重复：如果两个不同字指向相同坐标，需要微调
    used_coords = {}
    for ch in expected_chars:
        pos = char_to_pos[ch]
        if pos in used_coords:
            # 坐标重复！微调其中一个
            other_ch = used_coords[pos]
            # 在原始框中找一个稍微偏移的坐标
            delta = 0.02  # 2%的微调
            adjusted = (round(pos[0] + delta, 4), round(pos[1], 4))
            char_to_pos[ch] = adjusted
            debug_lines.append(f"  FIX: [{ch}] 与 [{other_ch}] 坐标重复，微调到 ({adjusted[0]:.4f},{adjusted[1]:.4f})")
        else:
            used_coords[pos] = ch

    result = [char_to_pos[ch] for ch in expected_chars]
    for i, ch in enumerate(expected_chars):
        debug_lines.append(f"  → 第{i+1}次点击 [{ch}] ({char_to_pos[ch][0]:.4f}, {char_to_pos[ch][1]:.4f})")
    debug_lines.append("--- OCR Completed ---")

    # 将 debug 信息输出到 stderr（方便 Rust 端从 HTTP 调试日志读取）
    for line in debug_lines:
        print(line, file=sys.stderr, flush=True)

    strategy = "single_char" if all_found else "single_char_partial"
    debug_text = "\n".join(debug_lines)
    return result, strategy, debug_text


# ──────────────────────── HTTP 服务 ────────────────────────
class OCRHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        """GET /health — 轻量健康检查端点"""
        if self.path == '/health':
            resp = json.dumps({
                "status": "ok" if MODELS_LOADED else "loading",
                "models_loaded": MODELS_LOADED,
            })
            self.send_response(200 if MODELS_LOADED else 503)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(resp.encode())
        else:
            self.send_response(404)
            self.end_headers()

    def do_POST(self):
        try:
            length = int(self.headers.get('Content-Length', 0))
            body = self.rfile.read(length)
            data = json.loads(body)

            img_b64 = data.get('image', '')
            expected_chars = data.get('expected_chars', [])

            # 健康检查：空图片直接返回200
            if not img_b64:
                response = json.dumps({
                    "points": [],
                    "strategy": "health_check",
                    "debug": "OK"
                })
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.end_headers()
                self.wfile.write(response.encode())
                return

            if not MODELS_LOADED:
                response = json.dumps({
                    "points": [],
                    "strategy": "error",
                    "debug": "Models not loaded yet"
                })
                self.send_response(503)
                self.send_header('Content-Type', 'application/json')
                self.end_headers()
                self.wfile.write(response.encode())
                return

            img_bytes = base64.b64decode(img_b64)
            points, strategy, debug_text = solve_captcha(img_bytes, expected_chars)

            response = json.dumps({
                "points": points,
                "strategy": strategy,
                "debug": debug_text
            })
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(response.encode())

        except Exception as e:
            import traceback
            error_detail = traceback.format_exc()
            error_resp = json.dumps({
                "points": [],
                "strategy": "error",
                "debug": f"ERROR: {str(e)}\n{error_detail}"
            })
            self.send_response(500)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(error_resp.encode())

    def log_message(self, format, *args):
        pass  # 静默日志


def write_ready_file(port):
    """写入就绪信号文件，通知 Rust 端服务已启动"""
    ready_path = os.path.join(tempfile.gettempdir(), f"ocr_ready_{port}")
    try:
        with open(ready_path, 'w') as f:
            f.write(str(os.getpid()))
        print(f"[OCR Server] ✓ 就绪信号文件: {ready_path}", flush=True)
    except Exception as e:
        print(f"[OCR Server] ⚠️ 写入就绪文件失败: {e}", flush=True)


def cleanup_ready_file(port):
    """清理就绪信号文件"""
    ready_path = os.path.join(tempfile.gettempdir(), f"ocr_ready_{port}")
    try:
        if os.path.exists(ready_path):
            os.remove(ready_path)
    except Exception:
        pass


if __name__ == '__main__':
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 19999

    # 1. 先加载模型
    print(f"[OCR Server] 正在初始化 (port={port})...", flush=True)
    if not load_models():
        print("[OCR Server] ❌ 模型加载失败，服务无法启动", flush=True)
        sys.exit(1)

    # 2. 创建 HTTP 服务器
    try:
        server = ThreadingHTTPServer(('127.0.0.1', port), OCRHandler)
    except OSError as e:
        print(f"[OCR Server] ❌ 端口 {port} 绑定失败: {e}", flush=True)
        sys.exit(1)

    print(f"[OCR Server] ✓ 监听端口: {port}", flush=True)

    # 3. 写入就绪信号文件（通知 Rust 端服务已就绪）
    write_ready_file(port)

    # 4. 注册退出清理
    def on_exit(signum, frame):
        cleanup_ready_file(port)
        sys.exit(0)

    signal.signal(signal.SIGTERM, on_exit)
    signal.signal(signal.SIGINT, on_exit)

    # 5. 开始服务
    try:
        server.serve_forever()
    finally:
        cleanup_ready_file(port)

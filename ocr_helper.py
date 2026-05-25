#!/usr/bin/env python3
"""OCR 子进程模式（作为 HTTP 服务的降级方案）

v3：与 ocr_server.py 保持一致的策略——单字识别为主，修复坐标重复bug
"""
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

def preprocess_aggressive(img):
    """更激进的预处理"""
    if img.mode == 'RGBA':
        img = img.convert('RGB')
    enh = ImageEnhance.Contrast(img)
    img = enh.enhance(2.0)
    enh = ImageEnhance.Sharpness(img)
    img = enh.enhance(3.0)
    img = img.filter(ImageFilter.SHARPEN)
    gray = img.convert('L')
    threshold = 128
    img = gray.point(lambda x: 255 if x > threshold else 0, '1')
    img = img.convert('RGB')
    return img

def filter_boxes(bboxes, iw, ih):
    """过滤检测框：去除过小/过大框，合并重叠框"""
    valid = []
    for box in bboxes:
        x1, y1, x2, y2 = box
        bw, bh = x2 - x1, y2 - y1
        if bw < 10 or bh < 10:
            continue
        ratio = bw / bh if bh > 0 else 999
        if ratio < 0.3 or ratio > 3.0:
            continue
        if bw > iw * 0.5 or bh > ih * 0.8:
            continue
        valid.append(box)

    # 合并重叠框
    valid.sort(key=lambda b: b[0])
    merged = []
    for box in valid:
        x1, y1, x2, y2 = box
        if not merged:
            merged.append(box)
            continue
        px1, py1, px2, py2 = merged[-1]
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
            merged[-1] = (min(x1, px1), min(y1, py1), max(x2, px2), max(y2, py2))
        else:
            merged.append(box)

    merged.sort(key=lambda b: b[0])
    return merged

# 常见形近字映射（与 ocr_server.py 一致）
CHAR_MAP = {
    '入': '人', '己': '已', '未': '末', '土': '士',
    '日': '曰', '千': '干', '尤': '优', '乃': '及',
    '戊': '戌', '申': '甲', '帅': '师', '辩': '辨',
    '博': '搏', '拨': '拔', '拔': '拨',
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
    '暖': '眼', '眼': '暖', '睛': '晴', '晴': '睛',
    '村': '林', '林': '村', '杜': '材', '材': '杜',
    '折': '拆', '拆': '折', '诉': '折', '听': '所',
    '所': '听', '新': '亲', '亲': '新', '立': '产',
    '产': '立', '庆': '广', '床': '广',
    '座': '广', '病': '广', '展': '居', '居': '展',
    '眉': '看', '省': '看', '看': '着', '着': '看',
    '样': '檬', '檬': '样', '校': '核', '核': '校',
    '检': '验', '验': '检',
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

# 构建反向映射
_REVERSE_CHAR_MAP = {}
for k, v in CHAR_MAP.items():
    if v not in _REVERSE_CHAR_MAP:
        _REVERSE_CHAR_MAP[v] = []
    _REVERSE_CHAR_MAP[v].append(k)

def map_char(ch, expected_chars):
    """将识别结果映射到候选集"""
    if ch in expected_chars:
        return ch
    if ch in CHAR_MAP and CHAR_MAP[ch] in expected_chars:
        return CHAR_MAP[ch]
    if ch in _REVERSE_CHAR_MAP:
        for rev in _REVERSE_CHAR_MAP[ch]:
            if rev in expected_chars:
                return rev
    for exp in expected_chars:
        if exp in CHAR_MAP and CHAR_MAP[exp] == ch:
            return exp
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

        # Detection
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
                print(f"{cx:.4f},{0.5:.4f},{expected_chars[i]}")
                log(f"  [{expected_chars[i]}] ({cx:.4f}, {0.5:.4f})")
            stop_kill_timer()
            return

        bboxes = filtered[:n]

        # ── 主策略：单字识别（双模型投票） ──
        ocr_beta = ddddocr.DdddOcr(beta=True, show_ad=False)
        ocr_default = ddddocr.DdddOcr(show_ad=False)
        ocr = ocr_beta  # prefer beta for single-char too

        char_to_pos = {}
        used_box_indices = set()

        for i, box in enumerate(bboxes):
            x1, y1, x2, y2 = box
            cx = (x1 + x2) / 2.0 / iw
            cy = (y1 + y2) / 2.0 / ih
            margin = 5

            # 双模型投票
            results = []
            for ocr_model in [ocr_beta, ocr_default]:
                crop = img.crop((max(0, x1 - margin), max(0, y1 - margin),
                               min(iw, x2 + margin), min(ih, y2 + margin)))
                crop_path = os.path.join(tmp_dir, f"char_{i}.png")
                crop.save(crop_path)
                with open(crop_path, 'rb') as f:
                    raw_ch = ocr_model.classification(f.read()).strip()
                results.append(raw_ch)

                # 激进预处理
                try:
                    crop2 = preprocess_aggressive(img.crop((max(0, x1 - margin), max(0, y1 - margin),
                                                            min(iw, x2 + margin), min(ih, y2 + margin))))
                    crop2_path = os.path.join(tmp_dir, f"char_{i}_aggressive.png")
                    crop2.save(crop2_path)
                    with open(crop2_path, 'rb') as f:
                        raw_ch2 = ocr_model.classification(f.read()).strip()
                    results.append(raw_ch2)
                except Exception:
                    pass

            # 投票
            from collections import Counter
            vote = Counter(results)
            best_ch, _ = vote.most_common(1)[0]

            # 定向候选集映射
            mapped_ch = map_char(best_ch, expected_chars)
            if mapped_ch is None:
                # 尝试所有结果
                for ch, _ in vote.most_common():
                    mapped_ch = map_char(ch, expected_chars)
                    if mapped_ch is not None:
                        break
            log(f"  单字识别: raw=[{best_ch}] mapped=[{mapped_ch}] -> box_{i}")

            if mapped_ch and mapped_ch in expected_chars and mapped_ch not in char_to_pos:
                char_to_pos[mapped_ch] = (cx, cy)
                used_box_indices.add(i)

        # ── 处理未识别的字（修复坐标重复bug！） ──
        unused_boxes = [(i, bboxes[i]) for i in range(len(bboxes)) if i not in used_box_indices]

        for ch in expected_chars:
            if ch in char_to_pos:
                continue
            if unused_boxes:
                idx, box = unused_boxes.pop(0)
                x1, y1, x2, y2 = box
                cx = (x1 + x2) / 2.0 / iw
                cy = (y1 + y2) / 2.0 / ih
                char_to_pos[ch] = (cx, cy)
                used_box_indices.add(idx)
                log(f"  WARNING: [{ch}] 未识别，使用未占用框{idx} ({cx:.4f},{cy:.4f})")
            else:
                char_to_pos[ch] = (0.5, 0.5)
                log(f"  WARNING: [{ch}] 未识别，无可用框，使用中心点")

        # 检查坐标重复并微调
        used_coords = {}
        for ch in expected_chars:
            pos = char_to_pos[ch]
            if pos in used_coords:
                delta = 0.02
                adjusted = (round(pos[0] + delta, 4), round(pos[1], 4))
                char_to_pos[ch] = adjusted
                log(f"  FIX: [{ch}] 与 [{used_coords[pos]}] 坐标重复，微调到 ({adjusted[0]:.4f},{adjusted[1]:.4f})")
            else:
                used_coords[pos] = ch

        # 输出结果
        for i, ch in enumerate(expected_chars):
            px, py = char_to_pos[ch]
            log(f"  → 第{i+1}次点击 [{ch}] ({px:.4f}, {py:.4f})")
            print(f"{px:.4f},{py:.4f},{ch}")

        log("--- OCR Completed ---")
        stop_kill_timer()

    except Exception as e:
        log(f"FATAL ERROR: {str(e)}")
        sys.exit(1)

if __name__ == "__main__":
    main()

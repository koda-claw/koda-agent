import base64
import os
import sys
from io import BytesIO
from pathlib import Path

import requests

DEFAULT_BACKEND = os.getenv("VISION_BACKEND") or os.getenv("KODA_VISION_BACKEND") or "openai"
DEFAULT_MAX_PIXELS = int(os.getenv("VISION_MAX_PIXELS") or os.getenv("KODA_VISION_MAX_PIXELS") or "1440000")


def ask_vision(image_input, prompt="详细描述这张图片的内容", timeout=60, max_pixels=DEFAULT_MAX_PIXELS, backend=DEFAULT_BACKEND):
    """GenericAgent-compatible vision helper backed by VISION_* env config."""
    _load_dotenv()
    try:
        prepared = _prepare_image(image_input, max_pixels)
    except Exception as e:
        return f"Error: 图片处理失败 - {type(e).__name__}: {e}"
    try:
        backend = (backend or DEFAULT_BACKEND or "openai").lower()
        if backend in ("claude", "messages"):
            return _call_claude(prepared, prompt, timeout)
        if backend in ("responses",):
            return _call_responses(prepared, prompt, timeout)
        if backend in ("openai", "chat", "modelscope"):
            return _call_openai_compat(prepared, prompt, timeout)
        return f"Error: 未知backend '{backend}'，可选: claude, openai, responses, modelscope"
    except requests.exceptions.Timeout:
        return f"Error: 请求超时 (>{timeout}s)"
    except requests.exceptions.RequestException as e:
        return f"Error: API请求失败 - {type(e).__name__}: {e}"
    except (KeyError, ValueError, TypeError) as e:
        return f"Error: 响应解析失败 - {e}"


def _prepare_image(image_input, max_pixels=DEFAULT_MAX_PIXELS):
    from PIL import Image

    if isinstance(image_input, Image.Image):
        img = image_input
    elif isinstance(image_input, (str, Path)):
        img = Image.open(image_input)
    else:
        raise TypeError(f"image_input must be a path or PIL Image, got {type(image_input).__name__}")
    w, h = img.size
    if w * h > max_pixels:
        scale = (max_pixels / (w * h)) ** 0.5
        img = img.resize((max(1, int(w * scale)), max(1, int(h * scale))), Image.Resampling.LANCZOS)
    if img.mode in ("RGBA", "LA", "P"):
        rgb = Image.new("RGB", img.size, (255, 255, 255))
        rgb.paste(img, mask=img.split()[-1] if img.mode == "RGBA" else None)
        img = rgb
    buf = BytesIO()
    img.save(buf, format="JPEG", quality=80, optimize=True)
    return {"mime": "image/jpeg", "base64": base64.b64encode(buf.getvalue()).decode("utf-8")}


def _call_openai_compat(image, prompt, timeout):
    base_url, api_key, model = _vision_cfg()
    headers = {"Content-Type": "application/json"}
    key_header = os.getenv("VISION_API_KEY_HEADER") or _infer_key_header(base_url) or "Authorization"
    if key_header.lower() == "authorization":
        headers["Authorization"] = f"Bearer {api_key}"
    else:
        headers[key_header] = api_key
    messages = []
    system = os.getenv("VISION_SYSTEM_PROMPT") or os.getenv("KODA_VISION_SYSTEM_PROMPT")
    if system:
        messages.append({"role": "system", "content": system})
    messages.append({
        "role": "user",
        "content": [
            {"type": "image_url", "image_url": {"url": f"data:{image['mime']};base64,{image['base64']}"}},
            {"type": "text", "text": prompt},
        ],
    })
    payload = {"model": model, "messages": messages}
    _apply_token_option(payload, base_url, responses=False)
    resp = requests.post(_make_url(base_url, "chat/completions"), json=payload, headers=headers, timeout=timeout, proxies=_proxies())
    resp.raise_for_status()
    return resp.json()["choices"][0]["message"]["content"]


def _call_responses(image, prompt, timeout):
    base_url, api_key, model = _vision_cfg()
    payload = {
        "model": model,
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": prompt},
                {"type": "input_image", "image_url": f"data:{image['mime']};base64,{image['base64']}"},
            ],
        }],
    }
    system = os.getenv("VISION_SYSTEM_PROMPT") or os.getenv("KODA_VISION_SYSTEM_PROMPT")
    if system:
        payload["instructions"] = system
    _apply_token_option(payload, base_url, responses=True)
    resp = requests.post(_make_url(base_url, "responses"), json=payload, headers=_bearer_headers(api_key), timeout=timeout, proxies=_proxies())
    resp.raise_for_status()
    data = resp.json()
    if data.get("output_text"):
        return data["output_text"]
    return "\n".join(
        block["text"]
        for item in data.get("output", [])
        for block in item.get("content", [])
        if block.get("type") in ("output_text", "text") and block.get("text")
    )


def _call_claude(image, prompt, timeout):
    base_url, api_key, model = _vision_cfg()
    payload = {
        "model": model,
        "max_tokens": int(os.getenv("VISION_MAX_TOKENS") or os.getenv("KODA_VISION_MAX_TOKENS") or "1024"),
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image", "source": {"type": "base64", "media_type": image["mime"], "data": image["base64"]}},
                {"type": "text", "text": prompt},
            ],
        }],
    }
    system = os.getenv("VISION_SYSTEM_PROMPT") or os.getenv("KODA_VISION_SYSTEM_PROMPT")
    if system:
        payload["system"] = system
    resp = requests.post(
        _make_url(base_url, "messages"),
        json=payload,
        headers={"x-api-key": api_key, "anthropic-version": "2023-06-01", "content-type": "application/json"},
        timeout=timeout,
        proxies=_proxies(),
    )
    resp.raise_for_status()
    return "\n".join(b["text"] for b in resp.json()["content"] if b.get("type") == "text")


def _vision_cfg():
    base_url = os.getenv("VISION_BASE_URL") or os.getenv("KODA_VISION_BASE_URL") or os.getenv("OPENAI_BASE_URL")
    api_key = os.getenv("VISION_API_KEY") or os.getenv("KODA_VISION_API_KEY") or os.getenv("OPENAI_API_KEY")
    model = os.getenv("VISION_MODEL") or os.getenv("KODA_VISION_MODEL") or os.getenv("OPENAI_MODEL")
    if not base_url or not api_key or not model:
        raise ValueError("VISION_BASE_URL/VISION_API_KEY/VISION_MODEL missing")
    return base_url, api_key, model


def _make_url(base_url, path):
    base = base_url.rstrip("/")
    if base.endswith("/" + path) or base.endswith("/v1/" + path):
        return base
    if base.endswith("/v1"):
        return base + "/" + path
    return base + "/v1/" + path


def _bearer_headers(api_key):
    return {"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"}


def _apply_token_option(payload, base_url, responses=False):
    tokens = os.getenv("VISION_MAX_TOKENS") or os.getenv("KODA_VISION_MAX_TOKENS")
    if not tokens and "xiaomimimo.com" in base_url:
        tokens = "1024"
    if not tokens:
        return
    key = os.getenv("VISION_TOKEN_PARAM") or os.getenv("KODA_VISION_TOKEN_PARAM")
    if not key:
        key = "max_output_tokens" if responses else ("max_completion_tokens" if "xiaomimimo.com" in base_url else "max_tokens")
    payload[key] = int(tokens)


def _infer_key_header(base_url):
    return "api-key" if "xiaomimimo.com" in base_url else None


def _proxies():
    proxy = os.getenv("VISION_PROXY") or os.getenv("KODA_VISION_PROXY") or os.getenv("OPENAI_PROXY")
    return {"http": proxy, "https": proxy} if proxy else None


def _load_dotenv():
    root = Path(os.getenv("KODA_AGENT_ROOT") or Path(__file__).resolve().parents[1])
    path = root / ".env"
    if not path.exists():
        return
    for line in path.read_text(encoding="utf-8", errors="ignore").splitlines():
        s = line.strip()
        if not s or s.startswith("#") or "=" not in s:
            continue
        k, v = s.split("=", 1)
        os.environ.setdefault(k.strip(), v.strip().strip('"').strip("'"))


if __name__ == "__main__":
    print(ask_vision(sys.argv[1] if len(sys.argv) > 1 else "screenshot.png"))

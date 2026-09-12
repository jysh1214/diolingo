#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "torch>=2.6",
#   "transformers>=4.51",
#   "accelerate>=1.0",
#   "bitsandbytes>=0.45; sys_platform == 'linux'",
# ]
# ///
"""Translate English subtitle lines with a local Qwen model (CUDA).

diolingo calls this for `--translator qwen`; it also works on its own:

    uv run --script translate_qwen.py --input in.json --output out.json

Input JSON:  {"title": "...", "target": "Traditional Chinese ...", "lines": ["...", ...]}
Output JSON: {"translations": ["...", ...]}  -- same length as "lines"

Lines go to the model in numbered chunks (--batch-lines) so each fragment is
translated with its neighbours as context; several chunks are generated in one
forward batch (--batch-prompts). The model is asked to echo each English line
next to its translation; an answer whose echo does not match its input line (a
sign that the model merged or shifted fragments) is discarded and that line is
retried on its own.
"""
from __future__ import annotations

import argparse
import difflib
import json
import re
import sys
import time

DEFAULT_MODEL = "Qwen/Qwen3-8B"

SYSTEM_PROMPT = """You are a professional subtitle translator. Translate English subtitle lines into {target}.
Rules:
- Output one line per input line, in the form `<number><TAB><original English line><TAB><translation>`, with the same numbers and the same English text as the input.
- Never merge, split, reorder or skip lines. Many lines are fragments cut mid-sentence: translate only the words of that fragment, even if it reads incomplete; the next line continues it. Do not finish the sentence early and do not carry words over into the next line.
- Keep names, product names, code identifiers, numbers and units as they are. Keep sound tags such as [Music] in square brackets, translated.
- Use natural spoken phrasing. No explanations, notes, quotes or markdown.

Example input:
1\thello everyone welcome back to the
2\tchannel today we're looking at
3\t[Music]
Example output:
1\thello everyone welcome back to the\t{ex1}
2\tchannel today we're looking at\t{ex2}
3\t[Music]\t{ex3}"""

NUMBER_RE = re.compile(r"^\s*(\d+)\s*[\t.):：、．]?\s*(.*)$")
CJK_RE = re.compile(r"[　-ヿ㐀-䶿一-鿿豈-﫿＀-￯]")
ECHO_MIN_RATIO = 0.75


def log(msg: str) -> None:
    print(f"[qwen] {msg}", file=sys.stderr, flush=True)


def system_prompt(target: str) -> str:
    simplified = "Simplified" in target or "简体" in target
    if simplified:
        ex = ("大家好，欢迎回到", "频道，今天我们要来看", "[音乐]")
    else:
        ex = ("大家好，歡迎回到", "頻道，今天我們要來看", "[音樂]")
    return SYSTEM_PROMPT.format(target=target, ex1=ex[0], ex2=ex[1], ex3=ex[2])


def build_messages(chunk: list[str], context: list[str], title: str, target: str) -> list[dict]:
    user = f"Video title: {title}\n\n"
    if context:
        user += "Preceding lines (context only, do not output):\n"
        user += "".join(c.replace("\n", " ") + "\n" for c in context) + "\n"
    user += "Translate these lines:\n"
    user += "".join(f"{i + 1}\t{line.replace(chr(10), ' ')}\n" for i, line in enumerate(chunk))
    return [
        {"role": "system", "content": system_prompt(target)},
        {"role": "user", "content": user},
    ]


def _norm(s: str) -> str:
    return re.sub(r"[^a-z0-9]+", "", s.lower())


def _latin_words(s: str) -> int:
    return len(re.findall(r"[A-Za-z]{2,}", s))


def parse_numbered(reply: str, chunk: list[str]) -> list[str | None]:
    """Fill one slot per input line from `n<TAB>english<TAB>translation` lines.

    Also accepts `n<TAB>translation`. A line is rejected (left None) when its
    echoed English does not match the input line, or when the translation of a
    normal English line contains no CJK characters.
    """
    n = len(chunk)
    slots: list[str | None] = [None] * n
    reply = re.sub(r"<think>.*?</think>", "", reply, flags=re.S)
    for line in reply.splitlines():
        line = line.strip()
        if not line or line.startswith("```"):
            continue
        m = NUMBER_RE.match(line)
        if not m:
            continue
        k, rest = int(m.group(1)), m.group(2)
        if not (1 <= k <= n) or slots[k - 1] is not None:
            continue
        parts = [p.strip() for p in rest.split("\t") if p.strip()]
        if not parts:
            continue
        src = chunk[k - 1].replace("\n", " ")
        if len(parts) >= 2:
            echo, text = parts[0], parts[-1]
            if difflib.SequenceMatcher(None, _norm(echo), _norm(src)).ratio() < ECHO_MIN_RATIO:
                continue
        else:
            text = parts[0]
            if _norm(text) == _norm(src):
                continue  # only the echo came back
        if _latin_words(src) >= 3 and not CJK_RE.search(text):
            continue  # not translated
        slots[k - 1] = text
    return slots


class MockGenerator:
    """No model: echoes the input lines. Used to test the diolingo plumbing."""

    def generate(self, batches: list[list[dict]], max_new_tokens: int) -> list[str]:
        out = []
        for messages in batches:
            body = messages[-1]["content"].split("Translate these lines:\n", 1)[1]
            out.append("\n".join(f"{k}\t{t}\t譯:{t}" for k, t in (l.split("\t", 1) for l in body.strip().splitlines())))
        return out


class HfGenerator:
    def __init__(self, model_id: str, device: str, dtype: str, quant: str) -> None:
        import torch
        from transformers import AutoModelForCausalLM, AutoTokenizer

        self.torch = torch
        if device == "cuda" and not torch.cuda.is_available():
            sys.exit("[qwen] CUDA is not available to this torch build; pass --device cpu or fix the install")
        torch_dtype = {
            "auto": torch.bfloat16 if device == "cuda" else torch.float32,
            "bf16": torch.bfloat16,
            "fp16": torch.float16,
            "fp32": torch.float32,
        }[dtype]
        kwargs: dict = {"device_map": device}
        if quant != "none":
            from transformers import BitsAndBytesConfig

            kwargs["quantization_config"] = BitsAndBytesConfig(
                load_in_4bit=quant == "4bit",
                load_in_8bit=quant == "8bit",
                bnb_4bit_compute_dtype=torch.bfloat16,
                bnb_4bit_quant_type="nf4",
            )
        log(f"loading {model_id} on {device} ({dtype}{'' if quant == 'none' else ', ' + quant}) ...")
        t0 = time.time()
        self.tok = AutoTokenizer.from_pretrained(model_id)
        self.tok.padding_side = "left"
        if self.tok.pad_token is None:
            self.tok.pad_token = self.tok.eos_token
        try:
            self.model = AutoModelForCausalLM.from_pretrained(model_id, dtype=torch_dtype, **kwargs)
        except TypeError:  # transformers < 4.56 spells it torch_dtype
            self.model = AutoModelForCausalLM.from_pretrained(model_id, torch_dtype=torch_dtype, **kwargs)
        self.model.eval()
        log(f"model ready in {time.time() - t0:.1f}s")

    def generate(self, batches: list[list[dict]], max_new_tokens: int) -> list[str]:
        texts = [
            self.tok.apply_chat_template(m, tokenize=False, add_generation_prompt=True, enable_thinking=False)
            for m in batches
        ]
        enc = self.tok(texts, return_tensors="pt", padding=True).to(self.model.device)
        with self.torch.inference_mode():
            out = self.model.generate(
                **enc,
                max_new_tokens=max_new_tokens,
                do_sample=False,
                repetition_penalty=1.05,
                pad_token_id=self.tok.pad_token_id,
            )
        new_tokens = out[:, enc["input_ids"].shape[1]:]
        return self.tok.batch_decode(new_tokens, skip_special_tokens=True)


def chunked(seq: list, size: int) -> list[list]:
    return [seq[i:i + size] for i in range(0, len(seq), size)]


def translate(gen, lines: list[str], title: str, target: str, batch_lines: int, batch_prompts: int,
              tokens_per_line: int, debug: bool = False) -> list[str]:
    result: list[str | None] = [None] * len(lines)
    chunks = [(start, lines[start:start + batch_lines]) for start in range(0, len(lines), batch_lines)]
    t0 = time.time()
    done = 0
    for group in chunked(chunks, batch_prompts):
        batches = [build_messages(chunk, lines[max(0, s - 3):s], title, target) for s, chunk in group]
        longest = max(len(c) for _, c in group)
        replies = gen.generate(batches, min(4096, tokens_per_line * longest + 32))
        for (start, chunk), reply in zip(group, replies):
            if debug:
                log(f"--- raw reply for lines {start + 1}-{start + len(chunk)} ---\n{reply}\n---")
            for i, t in enumerate(parse_numbered(reply, chunk)):
                result[start + i] = t
            done += len(chunk)
        log(f"translated {done}/{len(lines)} lines ({time.time() - t0:.1f}s)")

    missing = [i for i, t in enumerate(result) if t is None]
    if missing:
        log(f"{len(missing)} line(s) rejected or missing from batch output; retrying them one by one")
        for group in chunked(missing, batch_prompts):
            batches = [build_messages([lines[i]], lines[max(0, i - 3):i], title, target) for i in group]
            replies = gen.generate(batches, min(4096, tokens_per_line + 32))
            for i, reply in zip(group, replies):
                if debug:
                    log(f"--- raw retry reply for line {i + 1} ---\n{reply}\n---")
                result[i] = parse_numbered(reply, [lines[i]])[0]
    for i, t in enumerate(result):
        if t is None:
            log(f"warning: no translation for line {i + 1}: {lines[i]!r}")
    log(f"done: {len(lines)} lines in {time.time() - t0:.1f}s, {len(missing)} retried singly")
    return [t or "" for t in result]


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    ap.add_argument("--input", help="input JSON file (default: stdin)")
    ap.add_argument("--output", help="output JSON file (default: stdout)")
    ap.add_argument("--model", default=DEFAULT_MODEL, help=f"Hugging Face model id or local path (default {DEFAULT_MODEL})")
    ap.add_argument("--device", default="cuda", choices=["cuda", "cpu"])
    ap.add_argument("--dtype", default="auto", choices=["auto", "bf16", "fp16", "fp32"])
    ap.add_argument("--quant", default="none", choices=["none", "4bit", "8bit"], help="bitsandbytes quantisation (for models that do not fit in VRAM)")
    ap.add_argument("--batch-lines", type=int, default=16, help="subtitle lines per prompt (small keeps numbering aligned)")
    ap.add_argument("--batch-prompts", type=int, default=8, help="prompts generated together in one forward batch")
    ap.add_argument("--tokens-per-line", type=int, default=80, help="generation budget per subtitle line (echo + translation)")
    ap.add_argument("--mock", action="store_true", help="skip the model and echo the input (plumbing test)")
    ap.add_argument("--debug", action="store_true", help="print raw model replies to stderr")
    args = ap.parse_args()

    src = open(args.input, encoding="utf-8") if args.input else sys.stdin
    with src:
        payload = json.load(src)
    lines: list[str] = payload["lines"]
    title: str = payload.get("title", "")
    target: str = payload.get("target", "Traditional Chinese as used in Taiwan (台灣繁體中文，使用台灣慣用語)")

    gen = MockGenerator() if args.mock else HfGenerator(args.model, args.device, args.dtype, args.quant)
    translations = translate(gen, lines, title, target, max(1, args.batch_lines), max(1, args.batch_prompts),
                             args.tokens_per_line, args.debug)
    out = {"model": "mock" if args.mock else args.model, "translations": translations}
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            json.dump(out, f, ensure_ascii=False, indent=1)
    else:
        json.dump(out, sys.stdout, ensure_ascii=False)
        sys.stdout.write("\n")


if __name__ == "__main__":
    main()

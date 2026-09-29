"""Free-only guard for the OpenRouter client."""
import pytest

from fantasy_manager.llm.openrouter import LLMClient, LLMError, is_free_model


class _Settings:
    openrouter_api_key = "k"
    fm_llm_model = "openrouter/free"


def test_is_free_model():
    assert is_free_model("openrouter/free")
    assert is_free_model("qwen/qwen3.8-27b:free")
    assert not is_free_model("google/gemini-2.5-flash-lite")


def test_real_client_refuses_paid_model(monkeypatch):
    monkeypatch.delenv("FM_LLM_ALLOW_PAID", raising=False)
    monkeypatch.delenv("FM_LLM_FALLBACKS", raising=False)
    c = LLMClient(_Settings(), model="google/gemini-2.5-flash-lite")
    assert c.free_only
    with pytest.raises(LLMError, match="non-free"):
        c._create([{"role": "user", "content": "hi"}], 10, 0.0, False)


def test_paid_allowed_with_env(monkeypatch):
    monkeypatch.setenv("FM_LLM_ALLOW_PAID", "1")
    c = LLMClient(_Settings(), model="google/gemini-2.5-flash-lite")
    assert not c.free_only


def test_injected_sdk_is_not_guarded():
    class FakeSDK:
        pass
    c = LLMClient(_Settings(), sdk=FakeSDK(), model="anything/paid")
    assert not c.free_only


def test_client_side_fallback_on_retryable_error():
    """A rate-limited primary falls through to the next model; auth errors do not."""
    import openai
    import httpx2 as httpx

    calls: list[str] = []

    class _Completions:
        def create(self, **kw):
            calls.append(kw["model"])
            if kw["model"] == "a/primary:free":
                req = httpx.Request("POST", "https://openrouter.ai/api/v1/chat/completions")
                resp = httpx.Response(429, request=req, json={"error": {"message": "rate limited"}})
                raise openai.RateLimitError("rate limited", response=resp, body=None)

            class _Msg:
                content = '{"note": "ok"}'

            class _Choice:
                message = _Msg()
                finish_reason = "stop"

            class _Resp:
                choices = [_Choice()]

            return _Resp()

    class _Chat:
        completions = _Completions()

    class FakeSDK:
        chat = _Chat()

    c = LLMClient(_Settings(), sdk=FakeSDK(), model="a/primary:free", fallbacks=["b/backup:free"])
    out = c.complete("sys", "user", json_mode=True)
    assert "ok" in out
    assert calls == ["a/primary:free", "b/backup:free"]


def test_truncated_answer_falls_back():
    calls: list[str] = []

    def _resp(text):
        class _Msg:
            content = text

        class _Choice:
            message = _Msg()
            finish_reason = "stop"

        class _Resp:
            choices = [_Choice()]

        return _Resp()

    class _Completions:
        def create(self, **kw):
            calls.append(kw["model"])
            return _resp("Trade Quinn") if kw["model"] == "a/p:free" else _resp("A" * 200)

    class _Chat:
        completions = _Completions()

    class FakeSDK:
        chat = _Chat()

    c = LLMClient(_Settings(), sdk=FakeSDK(), model="a/p:free", fallbacks=["b/q:free"])
    assert len(c.complete("s", "u", min_chars=120)) == 200
    assert calls == ["a/p:free", "b/q:free"]

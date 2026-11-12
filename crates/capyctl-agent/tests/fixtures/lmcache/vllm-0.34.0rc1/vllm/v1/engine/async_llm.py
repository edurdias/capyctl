# Excerpts fetched 2026-11-11 from https://raw.githubusercontent.com/vllm-project/vllm/v0.34.0rc1/vllm/v1/engine/async_llm.py
# Apache-2.0; verbatim methods in synthetic class wrappers without upstream bases.
# T45 source-shape checks only, not qualification.
class AsyncLLM:
    async def sleep(
        self, level: int = 1, mode: PauseMode = "abort", reset_connector: bool = True
    ) -> None:
        if level >= 1:
            await self.renderer.clear_mm_cache_async()
        await self.engine_core.sleep_async(level, mode, reset_connector)

        if self.logger_manager is not None:
            self.logger_manager.record_sleep_state(1, level)



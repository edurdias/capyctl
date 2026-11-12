# Excerpts fetched 2026-11-11 from https://raw.githubusercontent.com/vllm-project/vllm/v0.34.0rc1/vllm/v1/engine/core_client.py
# Apache-2.0; verbatim methods in synthetic class wrappers without upstream bases.
# T45 source-shape checks only, not qualification.
class AsyncMPClient:
    async def sleep_async(
        self, level: int = 1, mode: PauseMode = "abort", reset_connector: bool = True
    ) -> None:
        await self.call_utility_async("sleep", level, mode, reset_connector)



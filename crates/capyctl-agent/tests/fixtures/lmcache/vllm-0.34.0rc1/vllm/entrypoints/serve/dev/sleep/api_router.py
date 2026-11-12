# Excerpts fetched 2026-11-11 from https://raw.githubusercontent.com/vllm-project/vllm/v0.34.0rc1/vllm/entrypoints/serve/dev/sleep/api_router.py
# Apache-2.0; verbatim function/decorator excerpt with unrelated module content omitted.
# T45 source-shape checks only, not qualification.
@router.post("/sleep")
async def sleep(
    raw_request: Request,
    level: Annotated[int, Query(ge=0, le=2)] = 1,
    mode: Annotated[PauseMode, Query()] = "abort",
    reset_connector: Annotated[bool, Query()] = True,
) -> JSONResponse:
    with sleep_mode_operation_metrics().record("sleep"):
        await engine_client(raw_request).sleep(level, mode, reset_connector)
    return JSONResponse(content={"status": "sleeping", "level": level})



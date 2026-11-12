# Excerpts fetched 2026-11-11 from https://raw.githubusercontent.com/vllm-project/vllm/v0.34.0rc1/vllm/v1/engine/core.py
# Apache-2.0; verbatim methods in synthetic class wrappers without upstream bases.
# T45 source-shape checks only, not qualification.
class EngineCore:
    def reset_prefix_cache(
        self, reset_running_requests: bool = False, reset_connector: bool = False
    ) -> bool:
        return self.scheduler.reset_prefix_cache(
            reset_running_requests, reset_connector
        )


    def _reset_caches(
        self,
        reset_running_requests: bool = True,
        reset_connector: bool = True,
    ) -> None:
        # reset_connector=True so external connectors clear alongside
        # local caches, matching the pause_generation(clear_cache=True)
        # contract. No-op when no connector is configured.
        if not self.reset_prefix_cache(
            reset_running_requests=reset_running_requests,
            reset_connector=reset_connector,
        ):
            raise RuntimeError("Failed to reset the KV connector cache.")
        self.reset_mm_cache()
        self.reset_encoder_cache()


    def _finish_pause(self, clear_cache: bool, reset_connector: bool) -> None:
        # The drain of a checkpoint prepare is complete. A failure leaves the
        # engine FAILED instead of serving, as nothing waits on this pause.
        preparing = self._checkpoint_state == CheckpointState.PREPARING
        try:
            # A completed pause promises an idle device: nothing else waits on
            # the last dummy batch an idle DP rank launches.
            self.model_executor.collective_rpc("synchronize_device")
            if clear_cache:
                self._reset_caches(reset_connector=reset_connector)
                self._set_kv_cache_reclaimable(True)
            if preparing:
                self.release_kv_cache_memory()
                self.collective_rpc("checkpoint_prepare")
                self.collective_rpc("synchronize_device")
                self._checkpoint_state = CheckpointState.PREPARED
        except Exception as e:
            if not preparing:
                raise
            self._fail_checkpoint(e)


    def pause_scheduler(
        self,
        mode: PauseMode = "abort",
        clear_cache: bool = True,
        reset_connector: bool = True,
    ) -> Future | None:
        """Pause generation; behavior depends on mode.

        All pause modes queue new adds -- "abort" and "keep" skip step();
        "wait" allows step() so in-flight requests can drain.

        - ``abort``: Set PAUSED_NEW, abort all requests, wait for abort
          outputs to be sent (when running with output_queue), optionally
          clear caches, then complete the returned Future.
        - ``wait``: Set PAUSED_NEW (queue adds, keep stepping); when drained,
          optionally clear caches, then complete the returned Future.
        - ``keep``: Set PAUSED_ALL; return a Future that completes when the
          output queue is empty.

        ``reset_connector`` decides whether clearing caches also resets the
        KV connector's cache.
        """
        if mode not in get_args(PauseMode):
            raise ValueError(f"Invalid pause mode: {mode}")
        if mode == "wait":
            raise ValueError("'wait' mode can't be used in inproc-engine mode")

        if mode == "abort":
            self.scheduler.finish_requests(None, RequestStatus.FINISHED_ABORTED)

        pause_state = PauseState.PAUSED_ALL if mode == "keep" else PauseState.PAUSED_NEW
        self.scheduler.set_pause_state(pause_state)
        if clear_cache and not reset_connector:
            # Pending connector stores still read the KV cache that a sleep
            # discards. Step until they finish, as EngineCoreProc does before
            # it completes a pause.
            while self.scheduler.has_requests() or self.batch_queue:
                _, model_executed = self.step_fn()
                self.post_step(model_executed)
                if not model_executed:
                    time.sleep(0.001)
        self._finish_pause(clear_cache, reset_connector)

        return None


    def sleep(
        self,
        level: int = 1,
        mode: PauseMode = "abort",
        reset_connector: bool = True,
    ) -> None | Future:
        """Put the engine to sleep at the specified level.

        Args:
            level: Sleep level.
                - Level 0: Pause scheduling only. Requests are still accepted
                           but not processed. No GPU memory changes.
                - Level 1: Offload model weights to CPU, discard KV cache.
                - Level 2: Discard model parameters and KV cache.
            mode: Pause mode - how to deal with any existing requests, see
                documentation of pause_scheduler method.
            reset_connector: Also reset the KV connector's cache; see
                ``LLM.sleep``.

        """
        if self._checkpoint_state != CheckpointState.RUNNING:
            raise RuntimeError(
                f"Cannot sleep while the checkpoint state is {self._checkpoint_state}"
            )
        # Pause scheduler before sleeping.
        clear_prefix_cache = level >= 1
        pause_future = self.pause_scheduler(
            mode=mode,
            clear_cache=clear_prefix_cache,
            reset_connector=reset_connector,
        )
        if level < 1:
            return pause_future

        # Level 1+: Delegate to executor for GPU memory management
        model_executor = self.model_executor
        if pause_future is None:
            model_executor.sleep(level)
            return None

        future = Future[Any]()

        def pause_complete(f: Future):
            try:
                f.result()  # propagate any exception
                future.set_result(model_executor.sleep(level))
            except Exception as e:
                future.set_exception(e)

        logger.info("Waiting for in-flight requests to complete before sleeping...")
        pause_future.add_done_callback(pause_complete)
        return future


class EngineCoreProc:
    def has_work(self) -> bool:
        """Returns true if the engine should be stepped."""
        return (
            self.engines_running
            or self.scheduler.has_requests()
            or bool(self.batch_queue)
        )


    def pause_scheduler(
        self,
        mode: PauseMode = "abort",
        clear_cache: bool = True,
        reset_connector: bool = True,
    ) -> Future | None:
        """Pause generation; behavior depends on mode.

        All pause modes queue new adds -- "abort" and "keep" skip step();
        "wait" allows step() so in-flight requests can drain.

        - ``abort``: Set PAUSED_NEW, abort all requests, wait for abort
          outputs to be sent (when running with output_queue), optionally
          clear caches, then complete the returned Future.
        - ``wait``: Set PAUSED_NEW (queue adds, keep stepping); when drained,
          optionally clear caches, then complete the returned Future.
        - ``keep``: Set PAUSED_ALL; return a Future that completes when the
          output queue is empty.
        """
        if mode not in get_args(PauseMode):
            raise ValueError(f"Invalid pause mode: {mode}")

        def engine_idle_callback(engine: "EngineCoreProc", future: Future[Any]) -> None:
            try:
                engine._finish_pause(clear_cache, reset_connector)
            except Exception as e:
                future.set_exception(e)
            else:
                future.set_result(None)

        if mode == "abort":
            aborted_reqs = self.scheduler.finish_requests(
                None, RequestStatus.FINISHED_ABORTED
            )
            self._send_abort_outputs(aborted_reqs)

        pause_state = PauseState.PAUSED_ALL if mode == "keep" else PauseState.PAUSED_NEW
        self.scheduler.set_pause_state(pause_state)

        if self._pause_complete():
            self._finish_pause(clear_cache, reset_connector)
            return None

        future = Future[Any]()
        self._idle_state_callbacks.append(partial(engine_idle_callback, future=future))
        return future


    def _pause_complete(self) -> bool:
        """Returns True if the pause has fully completed and the caller can
        return ``None`` synchronously; False if the pause is still pending
        and the caller should register an idle-state callback to finish it.
        """
        return not self.has_work()



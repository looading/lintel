(() => {
  const nativeInvoke = window.__rsAPI__;
  let sequence = 0;
  const pending = new Map();

  const commandError = value => {
    const message = typeof value === 'string' ? value : 'LINTEL_COMMAND_FAILED';
    const error = new Error(message);
    error.code = /^LINTEL_([A-Z_]+)/.exec(message)?.[1] ?? 'COMMAND_FAILED';
    return error;
  };

  const cancelNative = id => {
    try {
      nativeInvoke('__lintel.cancel', JSON.stringify({ id }), () => {}, () => {});
    } catch {}
  };

  const invoke = (command, input = null, { signal, timeoutMs = 15000 } = {}) => {
    if (!Number.isFinite(timeoutMs) || timeoutMs <= 0 || timeoutMs > 3600000) {
      return Promise.reject(commandError('LINTEL_INVALID_INPUT'));
    }
    if (typeof nativeInvoke !== 'function') {
      return Promise.reject(commandError('LINTEL_UNAVAILABLE'));
    }
    if (signal?.aborted) {
      return Promise.reject(commandError('LINTEL_CANCELLED'));
    }
    if (pending.size >= 64) {
      return Promise.reject(commandError('LINTEL_BUSY'));
    }
    const id = String(++sequence);
    let payload;
    try {
      payload = JSON.stringify({ __lintel: 'request-v1', id, input });
    } catch {
      return Promise.reject(commandError('LINTEL_INVALID_INPUT'));
    }
    return new Promise((resolve, reject) => {
      let settled = false;
      const finish = (succeeded, value) => {
        if (settled) return;
        settled = true;
        clearTimeout(timeout);
        signal?.removeEventListener('abort', abort);
        pending.delete(id);
        if (succeeded) resolve(value);
        else reject(commandError(value));
      };
      const abort = () => {
        cancelNative(id);
        finish(false, 'LINTEL_CANCELLED');
      };
      const timeout = setTimeout(() => {
        cancelNative(id);
        finish(false, 'LINTEL_TIMEOUT');
      }, timeoutMs);
      pending.set(id, abort);
      signal?.addEventListener('abort', abort, { once: true });
      try {
        nativeInvoke(command, payload, reply => {
          try {
            finish(true, JSON.parse(reply));
          } catch {
            finish(false, 'LINTEL_INVALID_RESPONSE');
          }
        }, error => finish(false, error));
      } catch (error) {
        finish(false, String(error));
      }
    });
  };

  window.addEventListener('pagehide', () => {
    for (const cancel of Array.from(pending.values())) cancel();
  });
  Object.defineProperty(window, 'lintel', { value: Object.freeze({ invoke }) });
})();

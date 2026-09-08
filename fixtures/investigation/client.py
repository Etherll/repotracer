def request(transport, settings, payload):
    return transport.send(payload, timeout=settings.request_timeout)

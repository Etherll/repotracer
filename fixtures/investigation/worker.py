from client import request


def run_job(transport, settings, payload):
    return request(transport, settings, payload)

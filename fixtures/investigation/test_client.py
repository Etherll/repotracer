from client import request
from config import Settings, load_settings


class RecordingTransport:
    def send(self, payload, timeout):
        self.timeout = timeout
        return payload


def test_timeout_reaches_transport():
    transport = RecordingTransport()
    assert request(transport, Settings(2.5), "job") == "job"
    assert transport.timeout == 2.5


def test_environment_override():
    assert load_settings({"REQUEST_TIMEOUT": "7"}).request_timeout == 7.0

from dataclasses import dataclass


@dataclass
class Settings:
    request_timeout: float = 5.0


def load_settings(env):
    return Settings(request_timeout=float(env.get("REQUEST_TIMEOUT", "5")))

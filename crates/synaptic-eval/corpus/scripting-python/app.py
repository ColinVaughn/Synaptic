from router import route


def validate(path):
    return len(path) > 0


def handle_request(path):
    if validate(path):
        return route(path)
    return 0

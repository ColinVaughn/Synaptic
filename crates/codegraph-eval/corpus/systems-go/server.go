package main

func validate(path string) bool {
	return len(path) > 0
}

func HandleRequest(path string) int {
	if validate(path) {
		return Route(path)
	}
	return 0
}

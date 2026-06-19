public class Server {
    private final Router router = new Router();

    public boolean validate(String path) {
        return path.length() > 0;
    }

    public int handleRequest(String path) {
        if (validate(path)) {
            return router.route(path);
        }
        return 0;
    }
}

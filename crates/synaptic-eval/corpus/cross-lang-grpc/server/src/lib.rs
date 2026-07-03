// tonic gRPC server for the Greeter service. say_hello is the labeled handler;
// wave belongs to a different service (Other) and must stay unconnected.
use tonic::{Request, Response, Status};

pub struct GreeterService;

#[tonic::async_trait]
impl Greeter for GreeterService {
    async fn say_hello(&self, req: Request<HelloRequest>) -> Result<Response<HelloReply>, Status> {
        Ok(Response::new(HelloReply::default()))
    }
}

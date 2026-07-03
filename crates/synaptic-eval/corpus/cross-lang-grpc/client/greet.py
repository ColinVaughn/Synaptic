# Python gRPC client. hello() talks to Greeter (the Rust tonic service);
# bye() talks to a service named Other that no server here implements.
import grpc
import greeter_pb2_grpc
import other_pb2_grpc


def hello(channel):
    stub = greeter_pb2_grpc.GreeterStub(channel)
    return stub.SayHello(None)


def bye(channel):
    stub = other_pb2_grpc.OtherStub(channel)
    return stub.SayBye(None)

FROM ubuntu:24.04

WORKDIR /app

RUN apt-get update && apt-get install -y ufw curl iputils-ping iproute2

COPY ./target/debug-release/irys /app/irys

RUN chmod +x /app/irys

ENTRYPOINT ["/app/irys"]

CMD [""]
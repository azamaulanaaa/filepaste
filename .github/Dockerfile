# Pin the digest so the base image does not change between builds.
FROM gcr.io/distroless/cc-debian12@sha256:e5d81ddde149641e2a9ba55be4545bc125c67de07508b03ba4c22e6eb0ded5aa

ARG BINARY_NAME

COPY ${BINARY_NAME} /usr/local/bin/app

ENTRYPOINT ["/usr/local/bin/app"]

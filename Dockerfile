# Todo: rest

FROM cgr.dev/chainguard/static
COPY --chown=nonroot:nonroot ./controller /app/
USER nonroot
EXPOSE 8080
ENTRYPOINT ["/app/controller"]

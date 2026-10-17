# Image du service hébergé : bike360-envoi (fonction Lambda, grâce à l'adaptateur web de Lambda),
# bike360-worker et l'outil d'analyse bike360-tool (tâche Fargate), avec ffmpeg.
# Contexte de construction : la racine du dépôt (voir cloud/deployer.sh).
#   docker build -f cloud/deploiement/Dockerfile -t bike360-cloud .

FROM rust:1-bookworm AS construction
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY core core
COPY server server
COPY render render
COPY cloud/service cloud/service
# l'outil d'analyse vient du cœur ; le moteur GPU (render/) n'est pas compilé ici
RUN cargo build --release --locked -p bike360-core --bin bike360-tool
RUN cargo build --release --locked --manifest-path cloud/service/Cargo.toml

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates ffmpeg && rm -rf /var/lib/apt/lists/*
# Adaptateur web de Lambda : dans une fonction, il confie les requêtes au serveur HTTP ; ailleurs, il ne fait rien.
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.1 /lambda-adapter /opt/extensions/lambda-adapter
COPY --from=construction /src/target/release/bike360-tool /src/cloud/service/target/release/bike360-envoi /src/cloud/service/target/release/bike360-worker /usr/local/bin/
ENV HOME=/tmp
ENTRYPOINT ["/usr/local/bin/bike360-envoi"]

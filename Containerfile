FROM registry.access.redhat.com/ubi10/ubi AS build
# flywire (default): bake the full FlyWire v783 brain (138,639 neurons, 15.1M connections) from the pinned upstream
# release below; synthetic: a 3,000-neuron toy network for quick builds. A ./brain directory with mapping.json overrides both.
ARG FLYOPS_BRAIN=flywire
ARG FLYWIRE_REPO=https://raw.githubusercontent.com/philshiu/Drosophila_brain_model/91bdd1e7dcf193f3e7ca5a8933497fcef63b7960
RUN dnf install -y --setopt=install_weak_deps=0 gcc rust cargo && dnf clean all
WORKDIR /src
COPY . .
RUN cargo build --release \
 && mkdir -p /brain \
 && if [ -f brain/mapping.json ]; then echo "using ./brain" && cp -r brain/. /brain/; \
    elif [ "$FLYOPS_BRAIN" = synthetic ]; then echo "baking the SYNTHETIC demo brain" && target/release/flyops --gen-synthetic-brain /brain; \
    else echo "baking FlyWire v783" \
      && curl -fsSL -o /brain/Completeness_783.csv "$FLYWIRE_REPO/Completeness_783.csv" \
      && curl -fsSL -o /brain/Connectivity_783.parquet "$FLYWIRE_REPO/Connectivity_783.parquet" \
      && (cd /brain && sha256sum -c /src/data/flywire-783/SHA256SUMS) \
      && cp data/flywire-783/mapping.json data/flywire-783/NOTICE /brain/; \
    fi

FROM registry.access.redhat.com/ubi10/ubi-minimal
COPY --from=build /src/target/release/flyops /usr/local/bin/flyops
# The brain travels with the operator: no volume needed.
COPY --from=build /brain /opt/flyops/brain
COPY LICENSE /licenses/LICENSE
USER 1001
EXPOSE 18787
ENTRYPOINT ["/usr/local/bin/flyops"]

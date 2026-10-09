#!/bin/bash
# Essai sur la machine AWS (à lancer en root une fois la préparation finie) :
#   sudo bash /opt/bike360/code/cloud/aws-essai/essai.sh
# Mesure ce que l'émulateur ne dit pas du montage Amazon S3 Files, puis la vitesse du moteur GPU.
# Les rushs à exporter se déposent avant dans s3://<compartiment>/rushs/ (un aperçu et son original).
# Rien n'arrête le script en route : chaque point est mesuré et le bilan est affiché à la fin.
set -u
. /opt/bike360/reglages
export AWS_DEFAULT_REGION
M=/mnt/bike360
CODE=/opt/bike360/code
T=$M/essai-$$
WAIT_S=180
bilan=()
note() { bilan+=("$1"); printf '   %s\n' "$1"; }
step() { printf '\n== %s\n' "$1"; }
now() { date +%s.%N; }
since() { echo "$(now) - $1" | bc -l | xargs printf '%.1f'; }

[ -f /opt/bike360/pret ] || { echo "préparation non terminée : voir /var/log/bike360-preparation.log"; exit 1; }
mountpoint -q $M || { echo "$M n'est pas monté"; exit 1; }
mkdir -p "$T"

step "Renommage et écriture atomique (le serveur écrit ses fichiers ainsi)"
echo '{"v":1}' > "$T/a.tmp" && mv "$T/a.tmp" "$T/a.json" && echo '{"v":2}' > "$T/b.tmp" && mv "$T/b.tmp" "$T/a.json"
[ "$(cat "$T/a.json" 2>/dev/null)" = '{"v":2}' ] && note "renommage par-dessus un fichier existant : oui" || note "renommage par-dessus un fichier existant : NON"

step "Délai du montage vers S3"
t0=$(now); key="essai-$$/a.json"; seen=non
for _ in $(seq 1 $WAIT_S); do aws s3api head-object --bucket "$BUCKET" --key "$key" >/dev/null 2>&1 && { seen=oui; break; }; sleep 1; done
[ $seen = oui ] && note "fichier écrit dans le montage visible dans S3 après $(since "$t0") s" || note "fichier écrit dans le montage toujours absent de S3 après $WAIT_S s"

step "Délai de S3 vers le montage, et classes de stockage lisibles"
head -c 4000000 /dev/urandom > /tmp/essai.bin
sum=$(md5sum < /tmp/essai.bin)
for class in STANDARD INTELLIGENT_TIERING GLACIER_IR DEEP_ARCHIVE; do
    aws s3 cp /tmp/essai.bin "s3://$BUCKET/essai-$$/$class.bin" --storage-class $class >/dev/null 2>&1 || { note "$class : dépôt refusé"; continue; }
    t0=$(now); seen=non
    for _ in $(seq 1 $WAIT_S); do [ -e "$T/$class.bin" ] && { seen=oui; break; }; sleep 1; done
    [ $seen = oui ] || { note "$class : objet toujours invisible dans le montage après $WAIT_S s"; continue; }
    delay=$(since "$t0")
    if [ "$(timeout 60 md5sum < "$T/$class.bin" 2>/dev/null)" = "$sum" ]; then note "$class : visible après $delay s, lu à l'identique"
    else note "$class : visible après $delay s, ILLISIBLE par le montage"; fi
done

step "Débit de lecture d'un original"
big=$(ls -S $M/rushs/*.insv 2>/dev/null | head -1)
if [ -n "$big" ]; then
    size=$(stat -c %s "$big")
    t0=$(now); dd if="$big" of=/dev/null bs=8M status=none; d=$(since "$t0")
    note "lecture par le montage : $(echo "$size / 1000000 / $d" | bc -l | xargs printf '%.0f') Mo/s ($(echo "$size / 1000000" | bc) Mo en $d s)"
    t0=$(now); aws s3 cp "s3://$BUCKET/rushs/$(basename "$big")" - > /dev/null; d=$(since "$t0")
    note "lecture directe de S3 : $(echo "$size / 1000000 / $d" | bc -l | xargs printf '%.0f') Mo/s"
else
    note "aucun original dans s3://$BUCKET/rushs/ : débit et moteur GPU non mesurés"
fi

# Lance l'atelier en mode hébergé sur un dossier de rushs, exporte 15 s en pleine qualité, renvoie « secondes moteur »
export_from() {
    local rushs=$1 data=/opt/bike360/essai-data-$RANDOM port=8399
    mkdir -p "$data"
    HOME=$data BIKE360_CLOUD=1 BIKE360_PASSWORD= BIKE360_ROOT=$data BIKE360_DATA=$data/data BIKE360_CACHE=$data/cache BIKE360_EXPORTS=$data/exports \
        "$CODE/target/release/bike360-server" "$rushs" --host 127.0.0.1 --port $port > "$data/serveur.log" 2>&1 &
    local pid=$! t0; t0=$(now)
    for _ in $(seq 1 600); do curl -sf -o /dev/null http://127.0.0.1:$port/api/sessions && break; sleep 1; done
    local start_s; start_s=$(since "$t0")
    local sid dur; sid=$(curl -s http://127.0.0.1:$port/api/sessions | jq -r 'max_by(.duration).id'); dur=$(curl -s http://127.0.0.1:$port/api/sessions | jq -r 'max_by(.duration).duration')
    local end=$(( dur > 18 ? 17 : dur - 1 ))
    curl -s -X PUT -H 'Content-Type: application/json' -d "[{\"id\":\"essai001\",\"start\":2,\"end\":$end,\"yaw\":0,\"pitch\":-10,\"fov\":100,\"horizon\":\"fixe\"}]" http://127.0.0.1:$port/api/selections/$sid >/dev/null
    t0=$(now); curl -s -X POST -H 'Content-Type: application/json' -d '{"quality":"final"}' http://127.0.0.1:$port/api/export/$sid >/dev/null
    local job
    for _ in $(seq 1 1800); do sleep 1; job=$(curl -s http://127.0.0.1:$port/api/export/$sid); [ "$(echo "$job" | jq -r .state)" = running ] || break; done
    local took; took=$(since "$t0")
    kill $pid 2>/dev/null
    echo "$(echo "$job" | jq -r .state) $(echo "$job" | jq -r .engine) $took $(( end - 2 )) $start_s $(echo "$job" | jq -r '.message // ""')"
}

step "Moteur GPU : export final de 15 s"
note "carte : $(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)"
if [ -n "$big" ]; then
    read -r state engine took clip start_s msg <<< "$(export_from $M/rushs)"
    if [ "$state" = done ]; then note "rushs sur le montage : $(echo "$took / $clip" | bc -l | xargs printf '%.2f') s par seconde de vidéo (moteur $engine, $clip s exportées en $took s ; analyse au démarrage : $start_s s)"
    else note "rushs sur le montage : export en échec ($msg)"; fi
    mkdir -p /opt/bike360/rushs-locaux && cp $M/rushs/* /opt/bike360/rushs-locaux/
    read -r state engine took clip start_s msg <<< "$(export_from /opt/bike360/rushs-locaux)"
    if [ "$state" = done ]; then note "rushs sur le disque local : $(echo "$took / $clip" | bc -l | xargs printf '%.2f') s par seconde de vidéo (moteur $engine)"
    else note "rushs sur le disque local : export en échec ($msg)"; fi
fi

rm -rf "$T"; aws s3 rm "s3://$BUCKET/essai-$$/" --recursive >/dev/null 2>&1
printf '\n== Bilan à rapporter (%s, %s)\n' "$(curl -s -H "X-aws-ec2-metadata-token: $(curl -s -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 60' http://169.254.169.254/latest/api/token)" http://169.254.169.254/latest/meta-data/instance-type)" "$AWS_DEFAULT_REGION"
printf '%s\n' "${bilan[@]}"

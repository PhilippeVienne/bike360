# Bike360 Cloud : socle de stockage

Infrastructure du service hébergé, décrite avec Terraform, et un essai local qui la déroule sur
[floci](https://floci.io), un émulateur d'AWS : pas de compte, pas de coût.

## Essai local

Prérequis : Docker, l'outil en ligne de commande `aws`, Terraform, et le serveur compilé
(`cargo build --release`).

```sh
sh cloud/essai.sh "/chemin/vers/LRV_….lrv"
docker compose -f cloud/compose.yml down    # arrêter l'émulateur
```

Le script crée le stockage, envoie l'aperçu par morceaux en simulant une coupure, relit sa
télémétrie par lecture partielle, fait passer une tâche dans la file, inscrit le rush dans l'index,
puis lance le serveur en mode hébergé (`BIKE360_CLOUD=1`) sur les rushs du client.

## Module Envoi

`cloud/envoi` est le service qui ouvre un envoi, signe l'adresse de chaque morceau et assemble le
fichier ; les vidéos vont directement du navigateur au stockage. Il a son propre espace Cargo, car
le kit AWS est long à compiler. La page est `ui/envoi.html`, son code d'envoi `ui/envoi-core.js`.

```sh
sh cloud/essai-envoi.sh "/chemin/vers/LRV_….lrv"    # demande aussi Node
```

L'essai fait tourner le code du navigateur contre le service : coupure après le premier morceau,
reprise des seuls morceaux manquants, puis contrôle que le fichier assemblé est identique.
À l'arrivée d'un rush, le service lit sa télémétrie par deux lectures partielles (caméra, durée
approchée d'après l'IMU), l'inscrit dans l'index (`--table`) et prévient l'atelier du client s'il
tourne (`--atelier`, jeton dans `BIKE360_ATELIER_TOKEN`).

Le client servi est fixé au lancement (`--client`) : l'authentification viendra avec le portail.

## Bibliothèque

Le même service sert la Bibliothèque (`cloud/envoi/src/library.rs`, page `ui/bibliotheque.html`) :
les rushs du client regroupés en balades d'après l'index, la place occupée, les marqueurs (favori,
garder, corbeille), l'allègement d'une session (ses originaux sont supprimés, son aperçu reste) et
la corbeille, vidée après un délai de garde (`--trash-days`, 30 jours par défaut).
`cloud/essai-envoi.sh` enchaîne l'essai de l'Envoi puis celui de la Bibliothèque.

## Ce que Terraform crée

| Ressource | Rôle |
| --- | --- |
| Compartiment S3 | Rushs et exports, rangés par nature puis par client : `apercus/<client>/`, `originaux/<client>/`, `exports/<client>/` |
| Règles d'archivage | Originaux en archive profonde après 90 jours, exports supprimés après 30 jours, envois abandonnés purgés après 7 jours |
| Règle CORS | Envoi direct depuis le navigateur |
| Table DynamoDB `bike360` | Index par client (`pk` = client, `sk` = nature et identifiant) |
| Files SQS `bike360-gpu` et `bike360-gpu-rebut` | Tâches du moteur GPU, et celles qui ont échoué trois fois |

La classe de stockage se choisit à l'envoi : Intelligent-Tiering pour les aperçus, Glacier Instant
Retrieval pour les originaux.

## Sur un compte AWS

```sh
cd cloud/terraform && terraform apply -var bucket=<nom-unique> -var site=https://<domaine>
```

Non testé à ce jour sur AWS.

## Ce que l'émulateur ne dit pas

Le montage Amazon S3 Files, la facturation et les délais des classes de stockage, CloudFront, le
GPU et les droits IAM ne sont pas émulés : ils restent à essayer sur un compte AWS.

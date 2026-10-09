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

## Essai à la main

```sh
sh cloud/demo.sh
```

Le script lance les émulateurs, l'infrastructure, le service avec les comptes, l'exécutant de tâches
et l'atelier à la demande, puis affiche l'adresse à ouvrir dans le navigateur et le parcours à
essayer. Les comptes créés sont confirmés d'office, puisqu'aucun courriel n'est envoyé ;
`sh cloud/demo.sh payer EMAIL PALIER` simule la confirmation d'un paiement. Ctrl-C arrête tout et
rien n'est conservé.

## Comptes

Les comptes reposent sur Amazon Cognito (`cloud/service/src/account.rs`, page `ui/compte.html`) :
inscription, confirmation de l'adresse par code, connexion. Le navigateur ne parle qu'au service,
qui pose les jetons dans des témoins que la page ne peut pas lire. Chaque requête agit pour le
compte connecté, dont l'identifiant sert de préfixe à ses rushs, ses résultats et ses lignes d'index.

Lancé sans émetteur (`--issuer`), le service sert un seul client (`--client`) et refuse d'écouter
ailleurs que sur sa machine.

## Paliers, quotas et paiement

Chaque compte a un palier (`cloud/service/src/plans.rs`) qui fixe sa place de stockage et ses
minutes d'export final par mois ; sans abonnement, c'est le palier d'essai. Un envoi qui ferait
dépasser la place est refusé, de même qu'un export final qui ferait dépasser les minutes : l'atelier
annonce chaque export final au service avant de le lancer. La grille par défaut se remplace par un
fichier (`--plans`).

Le paiement passe par Stripe (`cloud/service/src/payment.rs`, page `ui/palier.html`) : le service
ouvre une page de paiement, et seule la notification signée de Stripe change le palier du compte.
Les clés se donnent par variables d'environnement (`BIKE360_STRIPE_KEY`,
`BIKE360_STRIPE_WEBHOOK_SECRET`, `BIKE360_STRIPE_PRICES`). L'essai local utilise l'émulateur
`stripe-mock` et des notifications signées par le script.

## Module Envoi

`cloud/service` est le service qui ouvre un envoi, signe l'adresse de chaque morceau et assemble le
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



## Bibliothèque

Le même service sert la Bibliothèque (`cloud/service/src/library.rs`, page `ui/bibliotheque.html`) :
les rushs du client regroupés en balades d'après l'index, la place occupée, les marqueurs (favori,
garder, corbeille), l'allègement d'une session (ses originaux sont supprimés, son aperçu reste) et
la corbeille, vidée après un délai de garde (`--trash-days`, 30 jours par défaut).
`cloud/essai-envoi.sh` enchaîne les essais : comptes, paliers et paiement, Envoi, analyse à l'arrivée,
atelier à la demande et export final, Bibliothèque.

## Analyse à l'arrivée

À chaque aperçu reçu, le service dépose une tâche dans la file. `bike360-worker` (second exécutable
de `cloud/service`) la prend : il copie les aperçus de la session et les traces GPS du client sur
son disque, lance `bike360-tool arrivee` (analyse et vignette), dépose les résultats sous
`donnees/<client>/` et inscrit un résumé dans l'index. La Bibliothèque affiche alors la vignette,
la distance et les moments forts, et propose au nettoyage les sessions à l'arrêt ou sans moment fort.

Une session de plusieurs fichiers reçoit une tâche par fichier ; refaire l'analyse est sans effet.

## Atelier à la demande

Avec `--atelier-bin`, le service lance un `bike360-server` par compte, en mode hébergé, quand le
client ouvre son atelier depuis la Bibliothèque (`cloud/service/src/atelier.rs`). Le navigateur
reçoit une adresse à usage unique, valable une minute, qu'il échange contre une session : le mot de
passe de l'atelier n'est connu que du service. Un atelier inactif et sans calcul en cours est
enregistré puis arrêté (`--atelier-idle-min`, 30 minutes par défaut) ; ses clips, son projet et ses
réglages sont déposés sous `donnees/<client>/atelier/` et repris à l'ouverture suivante.

Les originaux ne sont amenés à l'atelier qu'au moment d'un export final : il les demande au service
(`BIKE360_ORIGINALS_URL`), en s'identifiant par son mot de passe. L'export terminé est déposé sous
`exports/<client>/` et proposé au téléchargement dans la Bibliothèque pendant 30 jours.

Ici, l'atelier est un processus sur la machine du service et les aperçus du client y sont recopiés.
Sur AWS, ce rôle reviendra à une tâche Fargate dont le stockage est monté.

## Ce que Terraform crée

| Ressource | Rôle |
| --- | --- |
| Compartiment S3 | Rushs, exports et résultats, rangés par nature puis par client : `apercus/<client>/`, `originaux/<client>/`, `exports/<client>/`, `donnees/<client>/` |
| Règles d'archivage | Originaux en archive profonde après 90 jours, exports supprimés après 30 jours, envois abandonnés purgés après 7 jours |
| Règle CORS | Envoi direct depuis le navigateur |
| Groupe d'utilisateurs Cognito | Comptes : une adresse de courriel confirmée, mot de passe d'au moins 10 caractères |
| Table DynamoDB `bike360` | Index par client (`pk` = client, `sk` = nature et identifiant) |
| Files SQS `bike360-gpu` et `bike360-gpu-rebut` | Tâches du moteur GPU, et celles qui ont échoué trois fois |

La classe de stockage se choisit à l'envoi : Intelligent-Tiering pour les aperçus, Glacier Instant
Retrieval pour les originaux.

## Sur un compte AWS

```sh
cd cloud/terraform && terraform apply -var bucket=<nom-unique> -var site=https://<domaine>
```

Non testé à ce jour sur AWS.

## Essai sur un compte AWS

`cloud/aws-essai` est un essai réduit, indépendant du reste : un compartiment monté avec Amazon S3
Files sur une machine GPU, pour mesurer ce que l'émulateur ne dit pas. Voir son
[mode d'emploi](aws-essai/README.md). À savoir dès maintenant : S3 Files exige les versions
d'objets sur le compartiment, donc une règle qui supprime les anciennes versions.

## Ce que l'émulateur ne dit pas

Le montage Amazon S3 Files, la facturation et les délais des classes de stockage, CloudFront, le
GPU, les droits IAM, l'envoi des courriels de confirmation et la politique de mot de passe ne sont
pas émulés ; le vrai parcours de paiement chez Stripe (carte, facture, résiliation) non plus : ils restent à essayer sur un compte AWS.

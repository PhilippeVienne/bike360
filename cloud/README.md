# Bike360 Cloud

Le service hébergé : son infrastructure décrite avec Terraform, le service lui-même
(`cloud/service`), et un essai local qui déroule le tout sur [floci](https://floci.io), un émulateur
d'AWS, et sur un faux serveur Mollie : pas de compte, pas de coût, aucun paiement.

Pour le mettre en ligne : [docs/deploiement-aws.md](../docs/deploiement-aws.md)
(`sh cloud/deployer.sh essai`). Le site vitrine est dans `site/`.

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
la page de paiement est celle du faux serveur Mollie, où l'on paie, échoue ou annule. Ctrl-C arrête
tout et rien n'est conservé.

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
dépasser la place est refusé, de même qu'un export final que ni les minutes du mois ni le crédit
acheté d'avance ne couvrent : l'atelier annonce chaque export final au service avant de le lancer. La grille par défaut se remplace par un
fichier (`--plans`).

### Paiement

Le prestataire de paiement est derrière une interface (`Provider`, `cloud/service/src/payment.rs`).
Celui du service est **Mollie** (`payment/mollie.rs`) ; Stripe (`payment/stripe.rs`) reste
disponible par `BIKE360_PAYMENT=stripe`, sans les écrans de moyen de paiement, et n'est plus essayé
de bout en bout.

Avec Mollie (`BIKE360_MOLLIE_KEY`, clé `test_…` ou `live_…`) :

- **Souscription** : un premier paiement sur la page de Mollie crée le mandat. À sa confirmation,
  le service crée chez Mollie un abonnement de douze mois qui commence un an plus tard, et la fiche
  du compte garde l'échéance payée (`echeance`).
- **Notifications** : Mollie n'envoie que l'identifiant du paiement. Le service relit ce paiement
  auprès de Mollie et n'agit que si c'est un paiement qu'il a lui-même ouvert, pour ce compte et ce
  montant (`pk = paiement#<tr_…>`, `sk = commande`). Chaque paiement n'est appliqué qu'une fois
  (`sk = recu`), même si la notification revient. La réponse ne dit jamais ce qui a été fait.
- **Renouvellement** : Mollie prélève et notifie ; l'échéance avance à la date que Mollie annonce.
  Un mois au moins avant, un courriel rappelle la reconduction et la façon de résilier.
- **Échec** : Mollie représente le prélèvement quelques jours. Le compte garde son palier pendant
  `--grace-days` (14) ; sans paiement, le passage des échéances met fin à l'abonnement.
- **Résiliation** : Mollie n'a pas de résiliation « à l'échéance ». L'abonnement est arrêté chez
  Mollie, et le compte garde son palier jusqu'à l'échéance payée ; y revenir recrée un abonnement
  qui commence à cette date.
- **Changement de palier** : un nouveau premier paiement, au prix entier, pour un an ; l'ancien
  abonnement est arrêté.
- **Crédit d'export** : paiement unique.
- **Contestation ou remboursement complet** : le crédit est retiré, ou l'abonnement terminé.
- **Moyen de paiement** : la page « Mon palier » montre la carte du mandat ; en changer passe par
  un premier paiement de 0 € par carte.
- **Reçus** : numérotés sans trou par année, avec la mention « TVA non applicable, article 293 B
  du CGI » et l'identité du vendeur (`BIKE360_VENDEUR`) ; page `ui/recu.html`, à imprimer.

`cloud/faux-mollie.mjs` tient lieu de Mollie dans les essais : mêmes appels, même forme de
notification, et des routes de pilotage (`/_faux/…`) pour payer, faire échouer, prélever une
échéance ou contester. `cloud/essai-paiement.mjs` déroule tout le parcours contre lui, et
`cargo test` vérifie la forme exacte des appels.

## Vie d'un compte

La fiche d'un compte (`sk = compte` dans l'index) porte son palier, ses dates et son crédit ; sa
situation s'en déduit (`plans::Standing`) et un passage régulier applique ce qu'elle demande
(`cloud/service/src/upkeep.rs`, toutes les `--sweep-min` minutes, 60 par défaut).

| Situation | Quand | Ce que le compte peut faire | Ce que fait le passage |
|---|---|---|---|
| essai | du premier envoi à `--trial-days` (7) | tout, dans les limites du palier d'essai | rien |
| essai terminé | ensuite, sans abonnement | s'abonner | supprime ses rushs |
| abonné | tant que l'abonnement court | tout | rien |
| terminé | `--access-days` (30) après la fin de l'abonnement | tout sauf envoyer | rien |
| archivé | ensuite, pendant `--archive-days` (180) | s'abonner, ce qui récupère ses rushs | étiquette ses rushs `etat=archive` ; une règle du compartiment les passe en archive profonde |
| supprimé | au-delà | s'abonner, sans retrouver ses rushs | supprime ses rushs |
| récupération | abonnement repris sur des rushs archivés | envoyer ; l'atelier attend | redemande les aperçus à l'archive, puis les remet dans leur classe |

- **Mot de passe oublié** : un code part par courriel (Cognito) ; la réponse est la même que le compte existe ou non.
- **Résiliation** : l'abonnement court jusqu'à son échéance, puis le compte revient au palier d'essai ;
  tant qu'elle n'est pas arrivée, la résiliation s'annule.
- **Récupération payante** : reprendre un palier sur des rushs archivés ajoute à la commande des
  frais par tranche de 100 Go (`--recovery-eur-100go`, 3 € par défaut, pour un coût d'environ 1,57 € :
  le mois d'accès, la garde en archive et la sortie d'archive au tarif de Stockholm). Les originaux restent en
  archive, comme tout original de plus de 90 jours.
- **Crédit d'export** : au-delà des minutes du mois, un export se paie d'avance, à la minute
  (`--credit-eur`, 0,07 € soit 4,20 € de l'heure ; `--credit-min`, 60 minutes au moins par achat, pour que la commission du
  prestataire de paiement ne mange pas la marge). Un
  export plus long que ce qu'il reste au compte ne démarre pas. Le crédit ne périme pas.
- **Dépassement du quota de stockage** : un abonné dépasse son quota si son crédit couvre le
  dépassement pendant `--overage-min-days` jours (7). Le temps passé au-dessus est pris sur le
  crédit, au prorata du volume et de la durée (`--overage-credits-100go`, 25 crédits par 100 Go et
  par 30 jours). Crédit épuisé : l'envoi est refusé, un courriel part, et le client a
  `--overage-grace-days` jours (30) pour régulariser ; ensuite ses rushs les plus anciens sont
  étiquetés pour l'archive (`gele` dans l'index) jusqu'à ce que le reste tienne dans le quota. Ils
  ne comptent plus dans le quota, se récupèrent en crédits (`POST /api/bibliotheque/recuperer`, au
  tarif de la récupération) et sont supprimés au bout de `--archive-days`.
- **Suppression du compte** : confirmée par le mot de passe ; le client est supprimé chez Mollie, ce qui
  arrête son abonnement et ses mandats, puis fichiers (toutes leurs versions), envois en cours, index et compte Cognito sont effacés.

`cloud/essai-echeances.mjs` déroule tout cela sur l'émulateur en reculant les dates dans l'index.

### Originaux sortis d'archive pour un export

Un original part en archive profonde à 90 jours. Quand un export en a besoin, le service demande
sa sortie d'archive (`--restore-tier bulk`, 48 h au plus, ou `standard`, 12 h), refuse l'export en
l'expliquant, et note l'attente dans l'index (`sk = sortie#<session>`). Le passage des échéances
voit l'original revenu : la Bibliothèque l'annonce, et un courriel part si un expéditeur est
configuré (`--mail-from`, par Amazon SES). L'original reste lisible `--restore-days` jours (3),
le temps de relancer l'export. Le coût de ces sorties d'archive n'est pas encore dans le chiffrage.

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

Une trace GPS déposée ou retirée dans l'atelier rejoint `donnees/<client>/gps/`, où l'analyse à
l'arrivée la lit aussi, et l'analyse des sessions de ces jours-là est redemandée.

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

`cloud/deploiement` reprend ce socle (module `cloud/terraform/socle`) et y ajoute le site
(S3, CloudFront), le service (Lambda, API Gateway), l'analyse à l'arrivée (Fargate) et les secrets :
voir [docs/deploiement-aws.md](../docs/deploiement-aws.md). `sh cloud/verifier-deploiement.sh` le
vérifie sans compte. Jamais déployé à ce jour.

## Essai sur un compte AWS

`cloud/aws-essai` est un essai réduit, indépendant du reste : un compartiment monté avec Amazon S3
Files sur une machine GPU, pour mesurer ce que l'émulateur ne dit pas. Voir son
[mode d'emploi](aws-essai/README.md). À savoir dès maintenant : S3 Files exige les versions
d'objets sur le compartiment, donc une règle qui supprime les anciennes versions.

## Ce que l'émulateur ne dit pas

Le montage Amazon S3 Files, la facturation et les délais des classes de stockage, CloudFront, le
GPU, les droits IAM, l'envoi des courriels de confirmation et la politique de mot de passe ne sont
pas émulés ; le vrai parcours de paiement chez Mollie (carte, mandat, abonnement, relances) non plus : ils restent à essayer sur un compte AWS,
avec une clé d'essai de Mollie.
L'émulateur n'applique pas les règles du compartiment, laisse lire un objet archivé et ne le rend
jamais : le passage en archive profonde, l'attente d'une sortie d'archive et la recopie des aperçus
revenus n'ont donc jamais tourné pour de bon (l'essai remet l'original à la main dans une classe
lisible). L'envoi de courriels par Amazon SES demande, sur un vrai compte, un domaine vérifié.

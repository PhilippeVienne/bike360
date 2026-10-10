# Déployer Bike360 Cloud sur AWS

Ce guide mène d'un compte AWS neuf à un site en ligne. Tout est décrit en Terraform
(`cloud/deploiement`) et se déploie par une seule commande, `sh cloud/deployer.sh`.

> **Jamais lancé sur un vrai compte.** Le déploiement est vérifié sans compte
> (`sh cloud/verifier-deploiement.sh` : mise en forme, validation, plans à blanc) et l'image se
> construit sur un poste. La première exécution réelle peut demander des retouches ; commencer par
> l'environnement d'essai.

## Ce qui est déployé

| Brique | Service AWS | Rôle |
| --- | --- | --- |
| Site vitrine et interface | S3 et CloudFront | `site/` à la racine, `ui/` sous `/ui/`, `/api/*` relayé au service |
| Service | Lambda derrière API Gateway | Comptes, envoi, bibliothèque, paliers, paiement (`bike360-envoi`, tel quel, grâce à l'adaptateur web de Lambda) |
| Échéances | Lambda déclenchée toutes les heures | Fin d'essai, archive, résiliation, renouvellement impayé, rappel de reconduction |
| Analyse à l'arrivée | Fargate (spot), lancé par la file | `bike360-worker` : vignette, distance, moments forts |
| Socle | S3, DynamoDB, SQS, Cognito | Le même module que l'essai local (`cloud/terraform/socle`) |
| Secrets | Parameter Store | Clé Mollie, chiffrée, hors du dépôt et hors de l'état Terraform |
| Garde-fous | Budgets, CloudWatch, SNS | Alerte de dépense, alerte quand le service échoue |

Tout est dans la région de Stockholm (`eu-north-1`), sauf le certificat du site, que CloudFront ne lit
qu'en Virginie du Nord. Rien ne tourne en permanence : sans visiteur, le coût se réduit au
stockage.

### Ce qui n'est pas encore déployé

**L'atelier de montage et l'export final.** Aujourd'hui, le service lance l'atelier comme un
processus sur sa propre machine, joint par le navigateur en HTTP sur un port par compte
(`cloud/service/src/atelier.rs`). Une fonction Lambda ne peut pas faire cela, et l'export final
demande une carte graphique. Sur AWS, la Bibliothèque annonce donc l'atelier comme indisponible.
Reste à écrire : le lancement de l'atelier en tâche Fargate avec le stockage monté, son accès en
HTTPS sous le domaine du site, et l'export sur machines GPU (file et AWS Batch). C'est le principal
chantier avant une mise en vente.

## Les deux environnements

| | `essai` | `production` |
| --- | --- | --- |
| Réglages | `cloud/deploiement/env/essai.tfvars` | `cloud/deploiement/env/production.tfvars` |
| Domaine | Aucun : adresse donnée par CloudFront | Le vôtre |
| Clé Mollie | `test_…` (aucun paiement réel) | `live_…` |
| Suppression | `sh cloud/deployer.sh essai detruire` | Protégée (table, comptes, dépôt d'images) |
| État Terraform | `essai/terraform.tfstate` | `production/terraform.tfstate` |

Les deux vivent dans le même compte AWS, sous des noms distincts (`bike360-essai-…`,
`bike360-production-…`).

## 1. Ce qu'il faut avoir

- Un compte AWS, avec un moyen de paiement.
- Un compte Mollie. Pour l'essai, la clé `test_…` suffit et s'obtient dès l'inscription. Pour la
  production, Mollie doit avoir validé l'activité (identité, compte bancaire, site en ligne).
- Pour la production : un nom de domaine, et l'identité du vendeur (nom, adresse, SIREN).
- Sur le poste : `aws` (version 2), `terraform` (1.10 ou plus), `docker`, `git`.

## 2. Donner les identifiants AWS

Créer dans IAM Identity Center (ou, à défaut, dans IAM) un utilisateur d'administration réservé au
déploiement, puis :

```sh
aws configure sso          # ou : aws configure, avec une clé d'accès
aws sts get-caller-identity
```

Ne jamais déployer avec le compte racine. Aucun identifiant n'entre dans le dépôt.

Sur un compte neuf, vérifier dans Service Quotas (région de Stockholm) que « Fargate Spot vCPU
resource count » vaut au moins 2.

## 3. Ranger la clé Mollie

```sh
sh cloud/secrets.sh essai        # demande la clé « test_… » au clavier, sans l'afficher
```

Le script refuse une clé `live_…` pour l'essai et une clé `test_…` pour la production. La clé est
rangée chiffrée sous `/bike360/<environnement>/MOLLIE_KEY` ; le service la lit au démarrage.

Rien n'est à régler chez Mollie pour les notifications : le service donne son adresse
(`https://<site>/api/paiement/mollie`) à chaque paiement qu'il ouvre.

## 4. Déployer l'essai

```sh
sh cloud/deployer.sh essai plan   # facultatif, après un premier déploiement : montre les changements
sh cloud/deployer.sh essai
```

La commande :

1. crée, la première fois, le compartiment de l'état Terraform (`bike360-etat-<compte>`) ;
2. construit l'image du service et la pousse dans le dépôt d'images (dix à vingt minutes la première fois) ;
3. applique l'infrastructure, après avoir montré le plan et demandé confirmation ;
4. dépose le site vitrine et l'interface, et vide le cache de CloudFront ;
5. contrôle que le site et le service répondent, et affiche l'adresse.

Elle se relance sans risque : seul ce qui a changé est touché.

## 5. Essayer le parcours

À l'adresse affichée : créer un compte (le code de confirmation arrive par courriel), envoyer un
rush, choisir un palier. La page de paiement de Mollie, en mode test, propose de simuler un
paiement réussi ou refusé. Au retour : palier, moyen de paiement, historique, reçu.

À vérifier à cette étape, parce que ni l'émulateur ni le faux serveur Mollie ne le disent :

- le premier paiement, puis l'abonnement créé chez Mollie (tableau de bord Mollie, « Abonnements ») ;
- la réponse de Mollie à l'arrêt d'un abonnement déjà arrêté et à la création d'un abonnement de
  même description (suppositions marquées dans `cloud/faux-mollie.mjs`) ;
- le changement de carte par un paiement de 0 € ;
- les droits IAM du service (une erreur « AccessDenied » dans les journaux
  `/aws/lambda/bike360-essai-service` désigne le droit manquant) ;
- le démarrage de l'analyse à l'arrivée quand un aperçu arrive, et son arrêt ensuite.

## 6. Passer en production

1. Compléter `cloud/deploiement/env/production.tfvars` : domaine, expéditeur des courriels,
   identité du vendeur, adresse d'alerte.
2. Faire relire et compléter les pages légales de `site/` (elles sont marquées « brouillon ») et
   la ligne de prix de la comparaison de la page d'accueil.
3. `sh cloud/secrets.sh production`, avec la clé `live_…`.
4. `sh cloud/deployer.sh production`.
5. Domaine hors de Route 53 : créer chez le registraire les enregistrements affichés
   (`terraform -chdir=cloud/deploiement output dns`), attendre que le certificat soit délivré,
   passer `certificate_ready = true` et relancer la commande.
6. Courriels : demander à AWS la sortie du « bac à sable » de SES (console SES, « Request
   production access »). D'ici là, SES n'écrit qu'aux adresses vérifiées, et les courriels des
   comptes partent par l'expéditeur par défaut de Cognito, limité à 50 par jour.
7. Refaire le parcours de l'étape 5 avec un vrai paiement du plus petit palier, puis le rembourser
   depuis le tableau de bord Mollie : l'abonnement du compte doit s'arrêter.

## Mettre à jour

```sh
sh cloud/deployer.sh production
```

Une clé Mollie changée (`sh cloud/secrets.sh …`) n'est prise qu'au redémarrage du service, donc
au déploiement suivant.

## Revenir en arrière

L'image de chaque déploiement porte la révision du code. Pour revenir à la précédente : se replacer
sur cette révision (`git checkout <révision>`) et relancer `sh cloud/deployer.sh <environnement>` ;
l'image, déjà dans le dépôt, n'est pas reconstruite.

## Vérifier sans compte AWS

```sh
sh cloud/verifier-deploiement.sh      # mise en forme, validation, plans à blanc des deux environnements
sh cloud/essai-envoi.sh APERÇU.lrv    # tout le service sur l'émulateur floci et le faux serveur Mollie
```

## Coûts fixes à connaître

Sans client, il reste : le stockage des images du service (quelques centimes), les journaux, la
zone Route 53 si le domaine y est (0,50 $ par mois), et le nom de domaine. Le reste est à l'usage :
voir le tableur de chiffrage. L'alerte de dépense (`alert_email`, `budget_usd`) prévient à 80 % du
plafond mensuel.

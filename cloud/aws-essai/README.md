# Essai réduit sur AWS : montage S3 Files et moteur GPU

L'émulateur local ne dit rien de quatre points dont dépend tout le service hébergé, ni de la
vitesse du moteur sur la carte d'AWS. Cet essai les mesure sur une seule machine, en une à deux
heures, puis tout se supprime.

| Question | Pourquoi elle compte |
| --- | --- |
| Le montage lit-il un original rangé en Glacier Instant ? Et en archive profonde ? | Tout le coût du stockage repose sur ces classes |
| Le renommage par-dessus un fichier fonctionne-t-il ? | Le serveur écrit ainsi ses clips et ses réglages |
| Combien de temps avant qu'un rush envoyé apparaisse dans le montage ? | Délai entre la fin d'un envoi et son apparition dans l'atelier |
| Quel débit en lecture pour un original ? | Durée d'un export sans copie locale |
| Combien de secondes de calcul par seconde de vidéo sur la carte T4 ? | Coût et délai d'un export, base du chiffrage (0,45 supposé) |

## Coût

Environ 0,18 $ par heure pour la machine au prix spot (0,62 $ sinon), plus quelques centimes de
disque et de stockage : moins d'un dollar si tout est supprimé au bout de deux heures. La machine
est facturée tant que `terraform destroy` n'a pas été lancé.

## Avant de commencer

- Un compte AWS et l'outil `aws` configuré (`aws configure`), avec le droit de créer des machines,
  des rôles et des compartiments.
- Le module [Session Manager](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html)
  de l'outil `aws` : il ouvre un terminal sur la machine sans port ouvert ni clé.
- Un quota de machines GPU : dans Service Quotas, « All G and VT Spot Instance Requests » doit
  valoir au moins 4 (processeurs virtuels). Sur un compte neuf il vaut souvent 0, et la demande
  d'augmentation peut prendre un jour. Sans spot (`-var spot=false`), c'est « Running On-Demand G
  and VT instances » qui compte.

## Déroulé

```sh
cd cloud/aws-essai
terraform init
terraform apply                      # crée le compartiment, le montage et la machine

# depuis votre ordinateur : un aperçu et son original (le plus petit couple suffit pour commencer)
B=$(terraform output -raw bucket)
aws s3 cp "/chemin/LRV_20260920_092259_01_581.lrv"  "s3://$B/rushs/"
aws s3 cp "/chemin/VID_20260920_092259_00_581.insv" "s3://$B/rushs/" --storage-class GLACIER_IR

$(terraform output -raw terminal)    # terminal sur la machine
```

Sur la machine, la préparation (outils, montage, compilation) prend une quinzaine de minutes :

```sh
sudo tail -f /var/log/bike360-preparation.log      # attendre la fin ; /opt/bike360/pret apparaît
sudo bash /opt/bike360/code/cloud/aws-essai/essai.sh
```

Le script affiche un « Bilan à rapporter » : ce sont ces lignes qu'il faut garder. Puis, sur votre
ordinateur :

```sh
terraform destroy                    # supprime tout, compartiment compris
```

## Ce que l'essai ne couvre pas

Fargate, CloudFront, les comptes et le paiement : ils viendront une fois ces mesures connues. Le
floutage n'est pas mesuré non plus (ses modèles ne sont pas installés sur la machine).

## Jamais lancé

Ce dossier est validé par `terraform validate`, mais n'a encore été appliqué sur aucun compte : la
première exécution peut demander des retouches (image de la machine, quota, zone de disponibilité).

<?php
// A synthetic Dolibarr page: permission check, a fired hook, and a raw query.
require '../main.inc.php';
dol_include_once('/dolibarr/class/widget.class.php');

if (!$user->hasRight('widgetshop', 'read')) {
    accessforbidden();
}

$title = $langs->trans('WidgetShelfLabel');

$hookmanager->initHooks(array('widgetcard'));
$parameters = array('id' => $id);
$reshook = $hookmanager->executeHooks('formObjectOptions', $parameters, $object, $action);

$sql = "SELECT w.rowid, w.label FROM llx_widgetshop_widget AS w WHERE w.status = 1";
$resql = $db->query($sql);

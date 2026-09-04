<?php
// A synthetic Dolibarr cron target class.
require_once DOL_DOCUMENT_ROOT.'/core/class/commonobject.class.php';

class WidgetShopCron extends CommonObject
{
    public function doScheduledJob($job)
    {
        // Revalidate widgets nightly.
        return 0;
    }
}
